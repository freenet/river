use super::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_stdlib::prelude::{ContractCode, ContractKey};
use river_core::room_state::ChatRoomParametersV1;
use river_core::room_state::ChatRoomStateV1;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::MemberId;
use river_core::room_state::privacy::RoomDisplayMetadata;
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
        rank: n as u32,
    }
}

/// Newest first, as `predecessors()` gives them.
fn preds(ns: &[u8]) -> Vec<Predecessor> {
    ns.iter().map(|n| pred(*n)).collect()
}

fn owner(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn contract_key(seed: u8, generation: u8) -> ContractKey {
    let params = ChatRoomParametersV1 {
        owner: owner(seed).verifying_key(),
    };
    ContractKey::from_params_and_code(
        freenet_stdlib::prelude::Parameters::from(cbor(&params)),
        ContractCode::from(vec![generation]),
    )
}

/// A slot as the web UI writes one, with the user's identity `me` and `extra` fields this app
/// does not read.
fn slot_with(seed: u8, me: u8, extra: Vec<(&str, Value)>) -> Vec<u8> {
    let o = owner(seed);
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
    let mut fields = vec![
        (
            Value::Text("owner_vk".into()),
            Value::serialized(&o.verifying_key()).unwrap(),
        ),
        (
            Value::Text("room_state".into()),
            Value::serialized(&state).unwrap(),
        ),
        (
            Value::Text("self_sk".into()),
            Value::serialized(&Some(owner(me))).unwrap(),
        ),
        (
            Value::Text("contract_key".into()),
            Value::serialized(&contract_key(seed, 1)).unwrap(),
        ),
        (
            Value::Text("self_nickname".into()),
            Value::Text(format!("me{me}")),
        ),
    ];
    for (k, v) in extra {
        fields.push((Value::Text(k.into()), v));
    }
    cbor(&Value::Map(vec![(
        Value::Text("Present".into()),
        Value::Map(fields),
    )]))
}

fn slot(seed: u8, me: u8) -> Vec<u8> {
    slot_with(seed, me, Vec::new())
}

fn tombstone() -> Vec<u8> {
    super::super::leave::tombstone_write(None).unwrap().unwrap()
}

fn key(seed: u8) -> Vec<u8> {
    room_storage_key(&owner(seed).verifying_key())
}

/// A predecessor's store as a chat delegate keeps it: `entries`, and the key index listing
/// them (in the listing's order).
fn store(entries: Vec<(Vec<u8>, Vec<u8>)>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut out: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();
    let keys: Vec<ChatDelegateKey> = out.keys().cloned().map(ChatDelegateKey::new).collect();
    #[derive(serde::Serialize)]
    struct Index {
        keys: Vec<ChatDelegateKey>,
    }
    if !keys.is_empty() {
        out.insert(KEY_INDEX_KEY.to_vec(), cbor(&Index { keys }));
    }
    out
}

/// A node's predecessor stores, by version number.
type Stores = BTreeMap<u8, BTreeMap<Vec<u8>, Vec<u8>>>;

/// What a predecessor answers to `ask`, from `store`. Its listing is what its index holds, as
/// the delegate's `ListRequest` reads it (the stored keys when it has none).
fn answer(store: &BTreeMap<Vec<u8>, Vec<u8>>, ask: &Ask) -> ChatDelegateResponseMsg {
    #[derive(serde::Deserialize)]
    struct Index {
        keys: Vec<ChatDelegateKey>,
    }
    match ask {
        Ask::List(_) => ChatDelegateResponseMsg::ListResponse {
            keys: match store.get(KEY_INDEX_KEY) {
                Some(b) => ciborium::de::from_reader::<Index, _>(b.as_slice())
                    .map(|i| i.keys)
                    .unwrap_or_default(),
                None => store.keys().cloned().map(ChatDelegateKey::new).collect(),
            },
        },
        Ask::Get(_, k) => ChatDelegateResponseMsg::GetResponse {
            key: ChatDelegateKey::new(k.clone()),
            value: store.get(k).cloned(),
        },
    }
}

/// How a predecessor fails, for [`walk_with`].
#[derive(Clone, Copy)]
enum Fault {
    /// It answers nothing it is asked (an error, or no answer): the walk stops there.
    Unreadable,
}

/// Run the walk against `stores` (a predecessor missing from it is not installed; one in
/// `faults` fails) and return what it would write, by room key: the identity of each room
/// written (`None` for a tombstone).
fn walk_reads(carry: &mut Carry, stores: &Stores, faults: &[(u8, Fault)]) {
    let mut asks = 0;
    while let Some(ask) = carry.next_ask() {
        asks += 1;
        assert!(asks < 1000, "the walk does not end");
        let n = ask.delegate().bytes()[0];
        if let Some((_, Fault::Unreadable)) = faults.iter().find(|(v, _)| *v == n) {
            carry.stop("refused".into());
            continue;
        }
        let Some(store) = stores.get(&n) else {
            assert!(carry.not_installed(ask.delegate()));
            continue;
        };
        assert!(carry.on_answer(&ask, ask.delegate(), &answer(store, &ask)));
    }
    assert!(carry.walked());
}

fn walk_with(
    carry: &mut Carry,
    stores: &Stores,
    faults: &[(u8, Fault)],
) -> BTreeMap<Vec<u8>, Option<VerifyingKey>> {
    walk_reads(carry, stores, faults);
    writes(carry).0
}

/// Make every write of a walk into an empty store: the slots written (by room key, the
/// identity of each room, `None` for a tombstone) and the ranks recorded.
fn writes(
    carry: &mut Carry,
) -> (
    BTreeMap<Vec<u8>, Option<VerifyingKey>>,
    Vec<(VerifyingKey, [u8; 32])>,
) {
    let mut out = BTreeMap::new();
    let mut ranks = Vec::new();
    while let Some(w) = carry.write() {
        match w {
            Write::Slot(k, bytes) => {
                let identity = match decode_room_slot(&bytes).unwrap() {
                    StoredRoomSlot::Present(r) => Some(r.self_verifying_key().unwrap()),
                    StoredRoomSlot::Tombstone => None,
                };
                out.insert(k, identity);
            }
            Write::AddRank(room, dk) => ranks.push((room, dk)),
            Write::RemoveRank(room) => ranks.retain(|(r, _)| *r != room),
        }
        carry.written(true);
    }
    (out, ranks)
}

fn walk(carry: &mut Carry, stores: &Stores) -> BTreeMap<Vec<u8>, Option<VerifyingKey>> {
    walk_with(carry, stores, &[])
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
    assert!(
        list.iter()
            .all(|p| &p.key != super::super::delegate::chat_delegate_key())
    );
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
    let stores: Stores = BTreeMap::from([(
        39,
        store(vec![(key(1), slot(1, 11)), (key(2), slot(2, 12))]),
    )]);
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    walk_reads(&mut c, &stores, &[]);
    let (out, ranks) = writes(&mut c);
    assert_eq!(out, BTreeMap::from([(key(1), me(11)), (key(2), me(12))]));
    assert_eq!(c.carried, 2);
    assert_eq!(c.not_installed, 1);
    assert!(c.stopped.is_none());
    // Each room's identity source is the delegate that supplied it.
    let mut sources = ranks;
    sources.sort_by_key(|(r, _)| r.to_bytes());
    let mut expected = vec![
        (owner(1).verifying_key(), [39u8; 32]),
        (owner(2).verifying_key(), [39u8; 32]),
    ];
    expected.sort_by_key(|(r, _)| r.to_bytes());
    assert_eq!(sources, expected);
}

#[test]
fn a_skipped_release_is_carried_from_older_generations() {
    // The user skipped every release since V38/V39: the newer predecessors are not installed.
    let stores: Stores = BTreeMap::from([
        (39, store(vec![(key(1), slot(1, 11))])),
        (38, store(vec![(key(2), slot(2, 12))])),
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
        (
            39,
            store(vec![(key(1), tombstone()), (key(2), slot(2, 22))]),
        ),
        // Older: room 1 present, room 2 under identity 12, room 3 present.
        (
            38,
            store(vec![
                (key(1), slot(1, 11)),
                (key(2), slot(2, 12)),
                (key(3), slot(3, 13)),
            ]),
        ),
        // Oldest: room 3 left long ago (older than its presence in V38).
        (37, store(vec![(key(3), tombstone())])),
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
    let stores: Stores = BTreeMap::from([(
        39,
        store(vec![(key(1), slot(1, 11)), (key(2), slot(2, 12))]),
    )]);
    let mut c = Carry::new(preds(&[39]), vec![key(1)], false);
    let mut gets = Vec::new();
    while let Some(ask) = c.next_ask() {
        if let Ask::Get(_, k) = &ask
            && parse_room_storage_key(k).is_some()
        {
            gets.push(k.clone());
        }
        assert!(c.on_answer(&ask, ask.delegate(), &answer(&stores[&39], &ask)));
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

/// The first write of a walk over `stores`.
fn first_write(stores: &Stores) -> Vec<u8> {
    let mut c = Carry::new(preds(&[39]), Vec::new(), true);
    while let Some(ask) = c.next_ask() {
        assert!(c.on_answer(&ask, ask.delegate(), &answer(&stores[&39], &ask)));
    }
    loop {
        match c.write().unwrap() {
            Write::Slot(_, bytes) => return bytes,
            _ => c.written(true),
        }
    }
}

fn fields_of(bytes: &[u8]) -> Vec<(Value, Value)> {
    let Value::Map(outer) = ciborium::de::from_reader::<Value, _>(bytes).unwrap() else {
        panic!("not a map");
    };
    let Some((_, Value::Map(fields))) = outer.into_iter().next() else {
        panic!("not a slot");
    };
    fields
}

#[test]
fn what_is_written_is_what_the_web_ui_reads_and_keeps_the_owed_room_contract_merge() {
    // The slot owes a room-contract merge from an older key, and was moved from another.
    let owed = Value::Map(vec![
        (
            Value::Text("owed".into()),
            Value::serialized(&vec![contract_key(1, 7)]).unwrap(),
        ),
        (
            Value::Text("unfinished_loads".into()),
            Value::Integer(2.into()),
        ),
    ]);
    let previous = Value::serialized(&Some(contract_key(1, 8))).unwrap();
    let raw = slot_with(
        1,
        11,
        vec![
            ("rekey_merge", owed.clone()),
            ("previous_contract_key", previous.clone()),
            // A field this app does not carry.
            ("own_purges", Value::Text("whatever".into())),
        ],
    );
    let bytes = first_write(&BTreeMap::from([(39, store(vec![(key(1), raw)]))]));
    let fields = fields_of(&bytes);
    let get = |name: &str| {
        fields
            .iter()
            .find(|(k, _)| k.as_text() == Some(name))
            .map(|(_, v)| v)
    };
    assert_eq!(get("rekey_merge"), Some(&owed));
    assert_eq!(get("previous_contract_key"), Some(&previous));
    assert!(get("own_purges").is_none());
    let StoredRoomSlot::Present(room) = decode_room_slot(&bytes).unwrap() else {
        panic!("not a room");
    };
    assert_eq!(room.self_nickname.as_deref(), Some("me11"));
    // The contract key it was stored under, so the web UI still sees the room contract moved.
    assert_eq!(room.contract_key, contract_key(1, 1));
}

#[test]
fn an_owed_merge_in_a_shape_the_web_ui_never_wrote_is_left_out() {
    let raw = slot_with(
        1,
        11,
        vec![(
            "rekey_merge",
            Value::Map(vec![(Value::Text("owed".into()), Value::Integer(1.into()))]),
        )],
    );
    let bytes = first_write(&BTreeMap::from([(39, store(vec![(key(1), raw)]))]));
    assert!(
        fields_of(&bytes)
            .iter()
            .all(|(k, _)| k.as_text() != Some("rekey_merge"))
    );
}

// An earlier generation that can't be read must never let an older one decide (review of #997).

#[test]
fn an_unreadable_newer_generation_stops_the_walk_and_nothing_older_is_carried() {
    let stores: Stores = BTreeMap::from([
        (40, store(vec![(key(1), slot(1, 11))])),
        (38, store(vec![(key(2), slot(2, 12))])),
    ]);
    let mut c = Carry::new(preds(&[40, 39, 38]), Vec::new(), true);
    let out = walk_with(&mut c, &stores, &[(39, Fault::Unreadable)]);
    // V40, newer than the one that failed, is carried; V38 is not even read.
    assert_eq!(out, BTreeMap::from([(key(1), me(11))]));
    assert_eq!(c.stopped.as_ref().map(|(v, _)| v.as_str()), Some("V39"));
}

#[test]
fn an_unread_rejoin_never_loses_to_an_older_identity() {
    // (a) In V39 the user joined the room again as 22; V38 holds the old identity 12. V39 can't
    // be read on this start: the old identity must not be written.
    let stores: Stores = BTreeMap::from([
        (39, store(vec![(key(1), slot(1, 22))])),
        (38, store(vec![(key(1), slot(1, 12))])),
    ]);
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    assert!(walk_with(&mut c, &stores, &[(39, Fault::Unreadable)]).is_empty());
    // Try again, with V39 readable: its identity is carried.
    c.restart();
    assert_eq!(walk(&mut c, &stores), BTreeMap::from([(key(1), me(22))]));
}

#[test]
fn an_older_tombstone_never_erases_a_room_an_unread_generation_holds() {
    // (b) V39 holds the room (with its signing key); V38 holds a tombstone from long ago.
    let stores: Stores = BTreeMap::from([
        (39, store(vec![(key(1), slot(1, 22))])),
        (38, store(vec![(key(1), tombstone())])),
    ]);
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    assert!(walk_with(&mut c, &stores, &[(39, Fault::Unreadable)]).is_empty());
    assert_eq!(c.tombstones, 0);
}

#[test]
fn a_room_left_in_an_unread_generation_never_comes_back() {
    // (c) The user left the room in V39; V38 still holds it.
    let stores: Stores = BTreeMap::from([
        (39, store(vec![(key(1), tombstone())])),
        (38, store(vec![(key(1), slot(1, 12))])),
    ]);
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    assert!(walk_with(&mut c, &stores, &[(39, Fault::Unreadable)]).is_empty());
}

#[test]
fn an_unreadable_or_mismatched_slot_holds_its_room_back_from_older_generations() {
    let stores: Stores = BTreeMap::from([
        // Newest: room 1's value can't be read, room 2's key holds room 3's slot, room 4 is fine.
        (
            39,
            store(vec![
                (key(1), b"garbage".to_vec()),
                (key(2), slot(3, 13)),
                (key(4), slot(4, 14)),
            ]),
        ),
        (
            38,
            store(vec![
                (key(1), slot(1, 11)),
                (key(2), slot(2, 12)),
                (key(5), slot(5, 15)),
            ]),
        ),
    ]);
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    let out = walk(&mut c, &stores);
    // Rooms 1 and 2 are held back (V38 may not decide them); the others are carried.
    assert_eq!(out, BTreeMap::from([(key(4), me(14)), (key(5), me(15))]));
    assert_eq!(c.held(), 2);
    assert!(c.stopped.is_none());
}

/// A walk of a single predecessor whose store is `s`, stopped or not, and its writes.
fn walk_one(s: BTreeMap<Vec<u8>, Vec<u8>>) -> (Carry, BTreeMap<Vec<u8>, Option<VerifyingKey>>) {
    let stores: Stores = BTreeMap::from([(39, s), (38, store(vec![(key(9), slot(9, 19))]))]);
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    let out = walk(&mut c, &stores);
    (c, out)
}

#[test]
fn a_key_index_that_does_not_add_up_stops_the_walk() {
    // The listing is short: the index names a room the listing leaves out.
    let mut s = store(vec![(key(1), slot(1, 11)), (key(2), slot(2, 12))]);
    #[derive(serde::Serialize)]
    struct Index {
        keys: Vec<ChatDelegateKey>,
    }
    s.insert(
        KEY_INDEX_KEY.to_vec(),
        cbor(&Index {
            keys: vec![ChatDelegateKey::new(key(1)), ChatDelegateKey::new(key(2))],
        }),
    );
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    while let Some(ask) = c.next_ask() {
        let a = match &ask {
            // The frozen delegate's listing swallowed part of its index.
            Ask::List(_) => ChatDelegateResponseMsg::ListResponse {
                keys: vec![ChatDelegateKey::new(key(1))],
            },
            _ => answer(&s, &ask),
        };
        assert!(c.on_answer(&ask, ask.delegate(), &a));
    }
    assert!(
        c.write().is_none(),
        "nothing from a short listing, and nothing older"
    );
    assert!(c.stopped.unwrap().1.contains("index"));

    // An index that does not decode.
    let mut s = store(vec![(key(1), slot(1, 11))]);
    s.insert(KEY_INDEX_KEY.to_vec(), b"garbage".to_vec());
    let (c, out) = walk_one(s);
    assert!(out.is_empty());
    assert!(c.stopped.is_some());

    // No index at all, while a room is listed.
    let mut s = store(vec![(key(1), slot(1, 11))]);
    s.remove(KEY_INDEX_KEY);
    let (c, _) = walk_one(s);
    assert!(c.stopped.is_some());
}

#[test]
fn two_reads_of_the_index_that_disagree_stop_the_walk() {
    let s = store(vec![(key(1), slot(1, 11))]);
    let mut c = Carry::new(preds(&[39]), Vec::new(), true);
    let mut index_reads = 0;
    while let Some(ask) = c.next_ask() {
        let mut a = answer(&s, &ask);
        if let (Ask::Get(_, k), ChatDelegateResponseMsg::GetResponse { value, .. }) = (&ask, &mut a)
            && k.as_slice() == KEY_INDEX_KEY
        {
            index_reads += 1;
            if index_reads == 2 {
                // A read fault (freenet-core#5857) on the second read.
                *value = None;
            }
        }
        assert!(c.on_answer(&ask, ask.delegate(), &a));
    }
    assert_eq!(index_reads, 2);
    assert!(c.stopped.unwrap().1.contains("disagree"));
}

#[test]
fn a_room_rooms_meta_names_but_the_index_leaves_out_stops_the_walk() {
    // The listing and the index agree, and hold only rooms_meta; room 1's slot is stored but
    // unlisted, and rooms_meta names it (the web UI's freenet/river#592 shape).
    #[derive(serde::Serialize)]
    struct Meta {
        room_order: Vec<VerifyingKey>,
    }
    let meta = cbor(&Meta {
        room_order: vec![owner(1).verifying_key()],
    });
    let mut s = store(vec![(ROOMS_META_KEY.to_vec(), meta)]);
    s.insert(key(1), slot(1, 11));
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    while let Some(ask) = c.next_ask() {
        assert_eq!(ask.delegate(), &pred(39).key, "nothing older is read");
        let a = match &ask {
            Ask::List(_) => ChatDelegateResponseMsg::ListResponse {
                keys: vec![ChatDelegateKey::new(ROOMS_META_KEY.to_vec())],
            },
            _ => answer(&s, &ask),
        };
        assert!(c.on_answer(&ask, ask.delegate(), &a));
    }
    // The room it proves is carried, as the web UI salvages it; nothing older is read.
    assert_eq!(writes(&mut c).0, BTreeMap::from([(key(1), me(11))]));
    assert!(c.stopped.unwrap().1.contains("index does not list"));
}

#[test]
fn a_listed_room_with_no_value_stops_the_walk_after_what_it_decided() {
    // Two rooms, the one read second answering no value (a read fault).
    let (first, second) = if key(1) < key(2) { (1, 2) } else { (2, 1) };
    let mut s = store(vec![
        (key(first), slot(first, 10 + first)),
        (key(second), slot(second, 10 + second)),
    ]);
    s.remove(&key(second));
    let (c, out) = walk_one(s);
    // The room decided by the generation's verified listing is safe to write; nothing older.
    assert_eq!(out, BTreeMap::from([(key(first), me(10 + first))]));
    assert!(c.stopped.is_some());
}

#[test]
fn a_predecessor_that_goes_missing_while_being_read_stops_the_walk() {
    let s = store(vec![(key(1), slot(1, 11))]);
    let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
    let ask = c.next_ask().unwrap();
    assert!(c.on_answer(&ask, ask.delegate(), &answer(&s, &ask)));
    assert!(c.not_installed(&pred(39).key));
    assert!(c.walked());
    assert!(c.stopped.is_some());
    assert_eq!(c.not_installed, 0);
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
    assert_eq!(get, Ask::Get(pred(39).key, KEY_INDEX_KEY.to_vec()));
    let wrong = ChatDelegateResponseMsg::GetResponse {
        key: ChatDelegateKey::new(key(2)),
        value: Some(slot(2, 12)),
    };
    assert!(!c.on_answer(&get, &pred(39).key, &wrong));
    // A listing while a key is awaited.
    assert!(!c.on_answer(&get, &pred(39).key, &list));
    assert_eq!(c.next_ask(), Some(get));
}

#[test]
fn try_again_walks_from_the_newest_again() {
    let stores: Stores = BTreeMap::from([(39, store(vec![(key(1), slot(1, 11))]))]);
    let mut c = Carry::new(preds(&[39]), Vec::new(), true);
    c.stop("no".into());
    assert!(c.walked());
    c.restart();
    assert!(c.stopped.is_none());
    let out = walk(&mut c, &stores);
    assert_eq!(out, BTreeMap::from([(key(1), me(11))]));
}

#[test]
fn a_request_to_a_predecessor_is_addressed_as_the_web_ui_addresses_it() {
    let p = pred(39);
    let ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages {
        key,
        params,
        inbound,
    }) = Ask::List(p.key.clone()).to_request()
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

// `rooms_meta`'s identity sources --------------------------------------------------------------

fn sources_of(bytes: &[u8]) -> Vec<(VerifyingKey, [u8; 32])> {
    #[derive(serde::Deserialize)]
    struct Meta {
        #[serde(default)]
        identity_sources: Vec<(VerifyingKey, [u8; 32])>,
    }
    ciborium::de::from_reader::<Meta, _>(bytes)
        .unwrap()
        .identity_sources
}

#[test]
fn identity_sources_are_added_replaced_and_every_other_field_kept() {
    let (a, b) = (owner(1).verifying_key(), owner(2).verifying_key());
    // No meta yet: one with only the sources.
    let fresh = meta_with_sources(None, &[(a, [39; 32])]).unwrap().unwrap();
    assert_eq!(sources_of(&fresh), vec![(a, [39; 32])]);
    // A meta the web UI wrote, with an older source for `a` and a field this app doesn't know.
    let existing = cbor(&Value::Map(vec![
        (
            Value::Text("room_order".into()),
            Value::serialized(&vec![b]).unwrap(),
        ),
        (
            Value::Text("identity_sources".into()),
            Value::serialized(&vec![(a, [5u8; 32])]).unwrap(),
        ),
        (Value::Text("future".into()), Value::Bool(true)),
    ]));
    let out = meta_with_sources(Some(&existing), &[(a, [39; 32]), (b, [38; 32])])
        .unwrap()
        .unwrap();
    let mut sources = sources_of(&out);
    sources.sort_by_key(|(r, _)| r.to_bytes());
    let mut expected = vec![(a, [39; 32]), (b, [38; 32])];
    expected.sort_by_key(|(r, _)| r.to_bytes());
    assert_eq!(sources, expected);
    let text = format!(
        "{:?}",
        ciborium::de::from_reader::<Value, _>(out.as_slice()).unwrap()
    );
    assert!(text.contains("future") && text.contains("room_order"));
    // Nothing to change: no write.
    assert_eq!(
        meta_with_sources(Some(&out), &[(a, [39; 32])]).unwrap(),
        None
    );
    assert_eq!(meta_with_sources(Some(&out), &[]).unwrap(), None);
    // Never over what can't be read.
    assert!(meta_with_sources(Some(b"garbage"), &[(a, [39; 32])]).is_err());
}

// Ranks: a predecessor's own `identity_sources` (review of #997, round 2) -----------------------

/// `rooms_meta` naming rooms in `order`, with these identity sources.
fn meta(order: &[u8], sources: &[(u8, u8)]) -> Vec<u8> {
    cbor(&Value::Map(vec![
        (
            Value::Text("room_order".into()),
            Value::serialized(
                &order
                    .iter()
                    .map(|s| owner(*s).verifying_key())
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
        ),
        (
            Value::Text("identity_sources".into()),
            Value::serialized(
                &sources
                    .iter()
                    .map(|(room, g)| (owner(*room).verifying_key(), [*g; 32]))
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
        ),
    ]))
}

/// N1: V40 holds the room under identity 11, which V40's web UI carried from V38 (its
/// `rooms_meta` says so); V39 holds the user's rejoin, identity 22. The web UI ranks V40's copy
/// at 38, so V39's wins; the carry must choose the same, and record V39's rank.
#[test]
fn a_copy_an_earlier_generation_carried_ranks_at_that_generation() {
    let stores: Stores = BTreeMap::from([
        (
            40,
            store(vec![
                (key(1), slot(1, 11)),
                (ROOMS_META_KEY.to_vec(), meta(&[1], &[(1, 38)])),
            ]),
        ),
        (39, store(vec![(key(1), slot(1, 22))])),
        (38, store(vec![(key(1), slot(1, 11))])),
    ]);
    let mut c = Carry::new(preds(&[40, 39, 38]), Vec::new(), true);
    walk_reads(&mut c, &stores, &[]);
    let (out, ranks) = writes(&mut c);
    assert_eq!(out, BTreeMap::from([(key(1), me(22))]));
    assert_eq!(ranks, vec![(owner(1).verifying_key(), [39; 32])]);
}

/// Without a newer copy, the provenance's rank is what is recorded: V40 carried it from V38.
#[test]
fn a_carried_copy_keeps_the_rank_its_generation_recorded() {
    let stores: Stores = BTreeMap::from([
        (
            40,
            store(vec![
                (key(1), slot(1, 11)),
                (ROOMS_META_KEY.to_vec(), meta(&[1], &[(1, 38)])),
            ]),
        ),
        (38, store(vec![(key(1), slot(1, 11))])),
    ]);
    let mut c = Carry::new(preds(&[40, 39, 38]), Vec::new(), true);
    walk_reads(&mut c, &stores, &[]);
    let (out, ranks) = writes(&mut c);
    assert_eq!(out, BTreeMap::from([(key(1), me(11))]));
    assert_eq!(ranks, vec![(owner(1).verifying_key(), [38; 32])]);
}

/// A tombstone at a rank above the carried copy's provenance wins (the room was left in V39).
#[test]
fn a_tombstone_newer_than_a_copys_provenance_wins() {
    let stores: Stores = BTreeMap::from([
        (
            40,
            store(vec![
                (key(1), slot(1, 11)),
                (ROOMS_META_KEY.to_vec(), meta(&[1], &[(1, 38)])),
            ]),
        ),
        (39, store(vec![(key(1), tombstone())])),
    ]);
    let mut c = Carry::new(preds(&[40, 39, 38]), Vec::new(), true);
    let out = walk(&mut c, &stores);
    assert_eq!(out, BTreeMap::from([(key(1), None)]));
}

/// A generation that can't be read and outranks a carried copy's provenance leaves the room
/// undecided: it may hold the newer copy.
#[test]
fn an_unread_generation_above_a_copys_provenance_holds_the_room_back() {
    let stores: Stores = BTreeMap::from([(
        40,
        store(vec![
            (key(1), slot(1, 11)),
            (ROOMS_META_KEY.to_vec(), meta(&[1], &[(1, 38)])),
        ]),
    )]);
    let mut c = Carry::new(preds(&[40, 39, 38]), Vec::new(), true);
    let out = walk_with(&mut c, &stores, &[(39, Fault::Unreadable)]);
    assert!(out.is_empty());
    assert_eq!(c.held(), 1);
}

/// The rooms `rooms_meta` names include its notification modes and identity sources
/// (`rooms_named_by_meta`): a room only those name, stored but unlisted, proves the index short.
/// The rooms it proves are carried (as the web UI salvages them) before the walk stops.
#[test]
fn a_room_named_only_by_modes_or_sources_proves_the_index_short_and_is_salvaged() {
    for field in ["notification_modes", "identity_sources"] {
        let value = match field {
            "notification_modes" => Value::Map(vec![(
                Value::serialized(&owner(1).verifying_key()).unwrap(),
                Value::Text("Mentions".into()),
            )]),
            _ => Value::serialized(&vec![(owner(1).verifying_key(), [38u8; 32])]).unwrap(),
        };
        let m = cbor(&Value::Map(vec![(Value::Text(field.into()), value)]));
        // Listing and index agree and hold only rooms_meta; the slot is stored unlisted.
        let mut s = store(vec![(ROOMS_META_KEY.to_vec(), m)]);
        s.insert(key(1), slot(1, 11));
        let stores: Stores = BTreeMap::from([(39, s), (38, store(vec![(key(2), slot(2, 12))]))]);
        let mut c = Carry::new(preds(&[39, 38]), Vec::new(), true);
        let out = walk(&mut c, &stores);
        assert_eq!(out, BTreeMap::from([(key(1), me(11))]), "{field}");
        assert!(
            c.stopped
                .as_ref()
                .unwrap()
                .1
                .contains("index does not list"),
            "{field}"
        );
    }
}

/// `rooms_data` and `outbound_dms` are probed even when unlisted (`candidate_keys`): stored but
/// unlisted proves the index short.
#[test]
fn a_fixed_key_stored_but_unlisted_stops_the_walk() {
    for fixed in [b"rooms_data".as_slice(), b"outbound_dms".as_slice()] {
        let mut s = store(vec![(key(1), slot(1, 11))]);
        s.insert(fixed.to_vec(), b"x".to_vec());
        let (c, out) = walk_one(s);
        assert!(out.is_empty());
        assert!(c.stopped.unwrap().1.contains("does not list it"));
    }
}

/// A room's rank goes to `rooms_meta` before its slot; a slot another writer got to first has
/// its rank taken out again.
#[test]
fn the_rank_is_written_before_the_slot_and_taken_out_when_the_slot_is_lost() {
    let stores: Stores = BTreeMap::from([(
        39,
        store(vec![(key(1), slot(1, 11)), (key(2), slot(2, 12))]),
    )]);
    let mut c = Carry::new(preds(&[39]), Vec::new(), true);
    walk_reads(&mut c, &stores, &[]);
    let first = c.write().unwrap();
    let Write::AddRank(room, dk) = first.clone() else {
        panic!("the rank first: {first:?}");
    };
    assert_eq!(dk, [39; 32]);
    assert_eq!(first.key(), ROOMS_META_KEY.to_vec());
    c.written(true);
    let Write::Slot(k, _) = c.write().unwrap() else {
        panic!("then the slot");
    };
    assert_eq!(k, room_storage_key(&room));
    // Another writer stored the room first: its rank is taken out.
    c.written(false);
    assert_eq!(c.write().unwrap(), Write::RemoveRank(room));
    c.written(true);
    assert_eq!(c.carried, 0);
    // The other room goes the same way, and is carried.
    assert!(matches!(c.write().unwrap(), Write::AddRank(..)));
    c.written(true);
    assert!(matches!(c.write().unwrap(), Write::Slot(..)));
    c.written(true);
    assert!(c.write().is_none());
    assert_eq!(c.carried, 1);
}

#[test]
fn a_rank_is_taken_out_with_every_other_field_kept() {
    let (a, b) = (owner(1).verifying_key(), owner(2).verifying_key());
    let m = meta(&[2], &[(1, 38), (2, 37)]);
    let out = meta_without_source(Some(&m), &a).unwrap().unwrap();
    assert_eq!(sources_of(&out), vec![(b, [37; 32])]);
    let text = format!(
        "{:?}",
        ciborium::de::from_reader::<Value, _>(out.as_slice()).unwrap()
    );
    assert!(text.contains("room_order"));
    // The last one out drops the field, as the web UI leaves it out when empty.
    let out = meta_without_source(Some(&out), &b).unwrap().unwrap();
    assert!(
        !format!(
            "{:?}",
            ciborium::de::from_reader::<Value, _>(out.as_slice()).unwrap()
        )
        .contains("identity_sources")
    );
    assert_eq!(meta_without_source(Some(&out), &b).unwrap(), None);
    assert_eq!(meta_without_source(None, &b).unwrap(), None);
    assert!(meta_without_source(Some(b"garbage"), &b).is_err());
}
