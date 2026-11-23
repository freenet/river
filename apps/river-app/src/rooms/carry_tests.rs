use super::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_stdlib::prelude::{ContractCode, ContractKey};
use river_core::room_state::ChatRoomParametersV1;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::MemberId;
use river_core::room_state::privacy::RoomDisplayMetadata;
use river_core::room_state::ChatRoomStateV1;
use river_core::stored_rooms::room_storage_key;

fn cbor<T: serde::Serialize>(v: &T) -> Vec<u8> {
    let mut b = Vec::new();
    ciborium::ser::into_writer(v, &mut b).unwrap();
    b
}

fn pred(n: u8) -> Predecessor {
    Predecessor {
        version: format!("V{n}"),
        key: DelegateKey::new([n; 32], CodeHash::new([n; 32])),
    }
}

/// Newest first, as `predecessors()` gives them.
fn preds(ns: &[u8]) -> Vec<Predecessor> {
    ns.iter().map(|n| pred(*n)).collect()
}

fn owner(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// A slot as the web UI writes one, including fields this app does not read (`own_purges`,
/// `rekey_merge`), with the user's identity `me`.
fn slot(seed: u8, me: u8) -> Vec<u8> {
    let o = owner(seed);
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: MemberId::from(&o.verifying_key()),
                display: RoomDisplayMetadata::public(format!("room {seed}"), None),
                ..Configuration::default()
            },
            &o,
        ),
        ..Default::default()
    };
    #[derive(serde::Serialize)]
    struct Data {
        owner_vk: VerifyingKey,
        room_state: ChatRoomStateV1,
        self_sk: Option<SigningKey>,
        contract_key: ContractKey,
        self_nickname: Option<String>,
        // A field the web UI writes in a shape this app never reads.
        rekey_merge: BTreeMap<String, u32>,
    }
    #[derive(serde::Serialize)]
    enum Slot {
        Present(Box<Data>),
    }
    cbor(&Slot::Present(Box::new(Data {
        owner_vk: o.verifying_key(),
        room_state: state,
        self_sk: Some(owner(me)),
        contract_key: ContractKey::from_params_and_code(
            freenet_stdlib::prelude::Parameters::from(cbor(&params)),
            ContractCode::from(vec![1u8]),
        ),
        self_nickname: Some(format!("me{me}")),
        rekey_merge: BTreeMap::from([("owed".to_owned(), 1)]),
    })))
}

fn tombstone() -> Vec<u8> {
    super::super::leave::tombstone_write(None).unwrap().unwrap()
}

fn key(seed: u8) -> Vec<u8> {
    room_storage_key(&owner(seed).verifying_key())
}

/// A node's predecessor stores, by version number.
type Stores = BTreeMap<u8, BTreeMap<Vec<u8>, Vec<u8>>>;

/// Run the walk against `stores` (a predecessor missing from it is not installed) and return
/// what it would write, by room key, the identity of each room written (`None` for a tombstone).
fn walk(carry: &mut Carry, stores: &Stores) -> BTreeMap<Vec<u8>, Option<VerifyingKey>> {
    let mut asks = 0;
    while let Some(ask) = carry.next_ask() {
        asks += 1;
        assert!(asks < 1000, "the walk does not end");
        let n = ask.delegate().bytes()[0];
        let Some(store) = stores.get(&n) else {
            assert!(carry.not_installed(ask.delegate()));
            continue;
        };
        let answer = match &ask {
            Ask::List(_) => ChatDelegateResponseMsg::ListResponse {
                keys: store.keys().cloned().map(ChatDelegateKey::new).collect(),
            },
            Ask::Get(_, k) => ChatDelegateResponseMsg::GetResponse {
                key: ChatDelegateKey::new(k.clone()),
                value: store.get(k).cloned(),
            },
        };
        assert!(carry.on_answer(&ask, ask.delegate(), &answer));
    }
    assert!(carry.walked());
    let mut out = BTreeMap::new();
    while let Some((k, bytes)) = carry.write() {
        let identity = match decode_room_slot(&bytes).unwrap() {
            StoredRoomSlot::Present(r) => Some(r.self_verifying_key().unwrap()),
            StoredRoomSlot::Tombstone => None,
        };
        out.insert(k, identity);
        carry.written(true);
    }
    out
}

fn me(seed: u8) -> Option<VerifyingKey> {
    Some(owner(seed).verifying_key())
}

#[test]
fn the_registry_parses_newest_first_with_every_row() {
    let list = predecessors();
    let rows = REGISTRY_TOML.matches("[[entry]]").count();
    assert_eq!(list.len(), rows);
    assert_eq!(list.last().unwrap().version, "V1");
    // The newest row is first, and it is the newest version in the file.
    let newest: u32 = list[0].version.trim_start_matches('V').parse().unwrap();
    for p in list {
        let n: u32 = p.version.trim_start_matches('V').parse().unwrap();
        assert!(n <= newest);
    }
    // V1's irregular key is the recorded one, not a derivation.
    assert_eq!(
        list.last().unwrap().key.bytes()[..4],
        [0x1a, 0x93, 0x30, 0x82]
    );
    // None is the delegate this build carries.
    assert!(list.iter().all(|p| &p.key != super::super::delegate::chat_delegate_key()));
}

#[test]
fn a_new_registry_row_is_walked_with_no_code_change() {
    let extra = format!(
        "{REGISTRY_TOML}\n[[entry]]\nversion = \"V99\"\ndescription = \"x\"\ndate = \"2026-12-01\"\n\
         delegate_key = \"{}\"\ncode_hash = \"{}\"\n",
        "ab".repeat(32),
        "cd".repeat(32)
    );
    let list = parse_registry(&extra).unwrap();
    assert_eq!(list[0].version, "V99");
    assert_eq!(list[0].key.bytes(), &[0xab; 32]);
}

#[test]
fn rooms_only_in_a_predecessor_are_carried_with_their_identity() {
    let stores: Stores = BTreeMap::from([(39, BTreeMap::from([(key(1), slot(1, 11)), (key(2), slot(2, 12))]))]);
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    let out = walk(&mut c, &stores);
    assert_eq!(out, BTreeMap::from([(key(1), me(11)), (key(2), me(12))]));
    assert_eq!(c.carried, 2);
    assert_eq!(c.not_installed, 1);
}

#[test]
fn a_skipped_release_is_carried_from_older_generations() {
    // The user skipped every release since V38/V39: the newer predecessors are not installed.
    let stores: Stores = BTreeMap::from([
        (39, BTreeMap::from([(key(1), slot(1, 11))])),
        (38, BTreeMap::from([(key(2), slot(2, 12))])),
    ]);
    let mut c = Carry::new(preds(&[41, 40, 39, 38, 37]), Vec::new(), true);
    let out = walk(&mut c, &stores);
    assert_eq!(out, BTreeMap::from([(key(1), me(11)), (key(2), me(12))]));
    assert_eq!(c.not_installed, 3);
}

#[test]
fn the_newest_generation_decides_identity_and_tombstones() {
    let stores: Stores = BTreeMap::from([
        // Newest: room 1 left, room 2 under identity 22.
        (39, BTreeMap::from([(key(1), tombstone()), (key(2), slot(2, 22))])),
        // Older: room 1 present, room 2 under identity 12, room 3 present.
        (
            38,
            BTreeMap::from([(key(1), slot(1, 11)), (key(2), slot(2, 12)), (key(3), slot(3, 13))]),
        ),
        // Oldest: room 3 left long ago (older than its presence in V38).
        (37, BTreeMap::from([(key(3), tombstone())])),
    ]);
    let mut c = Carry::new(preds(&[39, 38, 37]), Vec::new(), true);
    let out = walk(&mut c, &stores);
    assert_eq!(
        out,
        BTreeMap::from([(key(1), None), (key(2), me(22)), (key(3), me(13))])
    );
    assert_eq!((c.carried, c.tombstones), (2, 1));
}

#[test]
fn rooms_the_current_store_holds_are_never_read_or_carried() {
    // The successor is partly populated: room 1 is there (the user's own copy, or a tombstone),
    // room 2 is not.
    let stores: Stores =
        BTreeMap::from([(39, BTreeMap::from([(key(1), slot(1, 11)), (key(2), slot(2, 12))]))]);
    let mut c = Carry::new(preds(&[39]), vec![key(1)], false);
    let mut gets = Vec::new();
    while let Some(ask) = c.next_ask() {
        if let Ask::Get(_, k) = &ask {
            gets.push(k.clone());
        }
        let store = &stores[&39];
        let answer = match &ask {
            Ask::List(_) => ChatDelegateResponseMsg::ListResponse {
                keys: store.keys().cloned().map(ChatDelegateKey::new).collect(),
            },
            Ask::Get(_, k) => ChatDelegateResponseMsg::GetResponse {
                key: ChatDelegateKey::new(k.clone()),
                value: store.get(k).cloned(),
            },
        };
        assert!(c.on_answer(&ask, ask.delegate(), &answer));
    }
    assert_eq!(gets, vec![key(2)]);
}

#[test]
fn a_write_never_replaces_what_the_current_key_holds() {
    let bytes = slot(1, 11);
    assert_eq!(decide(None, &bytes), Some(bytes.clone()));
    assert_eq!(decide(Some(&[]), &bytes), Some(bytes.clone()));
    // Another writer (a browser tab, or the user's own tombstone) got there first: it wins.
    assert_eq!(decide(Some(&tombstone()), &bytes), None);
    assert_eq!(decide(Some(&slot(1, 99)), &bytes), None);
    // Even bytes this app can't read.
    assert_eq!(decide(Some(b"garbage"), &bytes), None);
}

#[test]
fn what_is_written_is_what_the_web_ui_reads_and_keeps_the_old_contract_key() {
    let stores: Stores = BTreeMap::from([(39, BTreeMap::from([(key(1), slot(1, 11))]))]);
    let mut c = Carry::new(preds(&[39]), Vec::new(), true);
    walk(&mut c, &stores);
    let mut c = Carry::new(preds(&[39]), Vec::new(), true);
    while let Some(ask) = c.next_ask() {
        let store = &stores[&39];
        let answer = match &ask {
            Ask::List(_) => ChatDelegateResponseMsg::ListResponse {
                keys: store.keys().cloned().map(ChatDelegateKey::new).collect(),
            },
            Ask::Get(_, k) => ChatDelegateResponseMsg::GetResponse {
                key: ChatDelegateKey::new(k.clone()),
                value: store.get(k).cloned(),
            },
        };
        c.on_answer(&ask, ask.delegate(), &answer);
    }
    let (_, bytes) = c.write().unwrap();
    // The field this app does not read is not copied (an older shape of it could stop the web
    // UI's reader); the fields it writes for any room are.
    let value: ciborium::Value = ciborium::de::from_reader(bytes.as_slice()).unwrap();
    let text = format!("{value:?}");
    assert!(!text.contains("rekey_merge"));
    let StoredRoomSlot::Present(room) = decode_room_slot(&bytes).unwrap() else {
        panic!("not a room");
    };
    assert_eq!(room.self_nickname.as_deref(), Some("me11"));
    // The contract key it was stored under, so the web UI still sees the room contract moved.
    let StoredRoomSlot::Present(orig) = decode_room_slot(&slot(1, 11)).unwrap() else {
        panic!()
    };
    assert_eq!(room.contract_key, orig.contract_key);
}

#[test]
fn unreadable_and_mismatched_slots_leave_the_room_to_an_older_generation() {
    let stores: Stores = BTreeMap::from([
        // Newest: room 1's value can't be read, room 2's key holds room 3's slot.
        (39, BTreeMap::from([(key(1), b"garbage".to_vec()), (key(2), slot(3, 13))])),
        (38, BTreeMap::from([(key(1), slot(1, 11)), (key(2), slot(2, 12))])),
    ]);
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    let out = walk(&mut c, &stores);
    assert_eq!(out, BTreeMap::from([(key(1), me(11)), (key(2), me(12))]));
}

#[test]
fn an_unreadable_predecessor_is_passed_over_and_remembered() {
    let stores: Stores = BTreeMap::from([(38, BTreeMap::from([(key(1), slot(1, 11))]))]);
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    let ask = c.next_ask().unwrap();
    assert_eq!(ask, Ask::List(pred(39).key));
    c.unreadable("refused".into());
    let out = walk(&mut c, &stores);
    assert_eq!(out, BTreeMap::from([(key(1), me(11))]));
    assert_eq!(c.unread, vec![("V39".to_owned(), "refused".to_owned())]);
}

#[test]
fn answers_from_another_delegate_or_for_another_key_are_not_taken() {
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    let ask = c.next_ask().unwrap();
    let list = ChatDelegateResponseMsg::ListResponse {
        keys: vec![ChatDelegateKey::new(key(1))],
    };
    // From V38 while V39 is asked.
    assert!(!c.on_answer(&ask, &pred(38).key, &list));
    // Not a not-installed for the one asked.
    assert!(!c.not_installed(&pred(38).key));
    assert!(c.on_answer(&ask, &pred(39).key, &list));
    let get = c.next_ask().unwrap();
    let wrong = ChatDelegateResponseMsg::GetResponse {
        key: ChatDelegateKey::new(key(2)),
        value: Some(slot(2, 12)),
    };
    assert!(!c.on_answer(&get, &pred(39).key, &wrong));
    assert_eq!(c.next_ask(), Some(get));
}

#[test]
fn try_again_walks_from_the_newest_again() {
    let stores: Stores = BTreeMap::from([(39, BTreeMap::from([(key(1), slot(1, 11))]))]);
    let mut c = Carry::new(preds(&[39]), Vec::new(), true);
    c.unreadable("no".into());
    assert!(c.walked());
    c.restart();
    assert!(c.unread.is_empty());
    let out = walk(&mut c, &stores);
    assert_eq!(out, BTreeMap::from([(key(1), me(11))]));
}

#[test]
fn a_request_to_a_predecessor_is_addressed_as_the_web_ui_addresses_it() {
    let p = pred(39);
    let ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { key, params, inbound }) =
        Ask::List(p.key.clone()).to_request()
    else {
        panic!("not an application message");
    };
    assert_eq!(key, p.key);
    assert!(params.as_ref().is_empty());
    let [InboundDelegateMsg::ApplicationMessage(m)] = inbound.as_slice() else {
        panic!("one message");
    };
    let msg: ChatDelegateRequestMsg = ciborium::de::from_reader(m.payload.as_slice()).unwrap();
    assert!(matches!(msg, ChatDelegateRequestMsg::ListRequest));
}
