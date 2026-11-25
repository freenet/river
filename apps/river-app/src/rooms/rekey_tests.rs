use super::*;
use ed25519_dalek::SigningKey;
use river_core::migration::contract_key_for_code_hash;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::MemberId;
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::RoomDisplayMetadata;
use river_core::room_state::upgrade::{AuthorizedUpgradeV1, UpgradeV1};
use std::time::{Duration, SystemTime};

fn owner() -> SigningKey {
    SigningKey::from_bytes(&[42; 32])
}

fn cbor<T: serde::Serialize>(v: &T) -> Vec<u8> {
    let mut b = Vec::new();
    ciborium::ser::into_writer(v, &mut b).unwrap();
    b
}

/// A real room (its configuration signed by its owner) holding `texts`.
fn room(texts: &[&str]) -> ChatRoomStateV1 {
    let o = owner();
    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: MemberId::from(&o.verifying_key()),
                display: RoomDisplayMetadata::public("Book club".into(), None),
                ..Configuration::default()
            },
            &o,
        ),
        ..Default::default()
    };
    let now = SystemTime::now();
    state.recent_messages.messages = texts
        .iter()
        .enumerate()
        .map(|(i, t)| {
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: MemberId::from(&o.verifying_key()),
                    author: MemberId::from(&o.verifying_key()),
                    time: (now - Duration::from_secs(100 - i as u64)).into(),
                    content: RoomMessageBody::public((*t).to_owned()),
                },
                &o,
            )
        })
        .collect();
    state
}

fn texts(state: &ChatRoomStateV1) -> Vec<String> {
    let mut out: Vec<String> = state
        .recent_messages
        .messages
        .iter()
        .filter_map(|m| m.message.content.as_public_string())
        .collect();
    out.sort();
    out
}

fn stored_under(key: ContractKey) -> StoredRoom {
    StoredRoom {
        owner_vk: owner().verifying_key(),
        room_state: room(&[]),
        self_sk: None,
        self_vk: None,
        contract_key: key,
        last_read_message_id: None,
        self_nickname: None,
        invitation_secrets: Default::default(),
        self_authorized_member: None,
        invite_chain: Vec::new(),
        self_member_info: None,
    }
}

fn earlier() -> Vec<ContractKey> {
    legacy_contract_keys_for_owner(&owner().verifying_key())
}

#[test]
fn a_room_under_an_earlier_key_is_followed_under_the_current_one() {
    let current = create::contract_key(&owner().verifying_key());
    for old in [earlier()[0], earlier()[5], *earlier().last().unwrap()] {
        assert_eq!(current_key(&stored_under(old)), (current, Some(old)));
    }
    // Already current.
    assert_eq!(current_key(&stored_under(current)), (current, None));
    // A key this build does not know (a newer River's): kept, never moved back.
    let newer = contract_key_for_code_hash(&owner().verifying_key(), &[7; 32]);
    assert_eq!(current_key(&stored_under(newer)), (newer, None));
}

#[test]
fn the_earlier_keys_are_read_newest_first_down_to_the_stored_one() {
    let all: Vec<ContractInstanceId> = earlier().iter().map(|k| *k.id()).collect();
    let o = owner().verifying_key();
    assert_eq!(candidates_down_to(&o, None), all);
    assert_eq!(
        candidates_down_to(&o, Some(&earlier()[0])),
        all[..1].to_vec()
    );
    assert_eq!(
        candidates_down_to(&o, Some(&earlier()[3])),
        all[..4].to_vec()
    );
    // A stored key that is not an earlier one: every earlier key.
    let other = contract_key_for_code_hash(&o, &[7; 32]);
    assert_eq!(candidates_down_to(&o, Some(&other)), all);
}

/// Run `probe` against `states` (by key; a key not there is absent) and return what it found
/// and what it read.
fn run(
    mut probe: Probe,
    states: &[(ContractInstanceId, Option<Vec<u8>>)],
) -> (Found, Vec<ContractInstanceId>) {
    let mut read = Vec::new();
    loop {
        match probe.next_read() {
            Ok(id) => {
                read.push(id);
                match states.iter().find(|(k, _)| *k == id) {
                    Some((_, Some(bytes))) => probe.state(id, bytes),
                    Some((_, None)) => probe.unknown(id),
                    None => probe.absent(id),
                }
            }
            Err(found) => return (found, read),
        }
    }
}

#[test]
fn the_newest_key_holding_a_real_room_wins() {
    let all = candidates_down_to(&owner().verifying_key(), None);
    let states = vec![
        // The newest earlier key holds something that is not a room, the next a room no owner
        // signed (a placeholder), the next is real, an older one is real too.
        (all[0], Some(b"garbage".to_vec())),
        (all[1], Some(cbor(&ChatRoomStateV1::default()))),
        (all[2], Some(cbor(&room(&["newer"])))),
        (all[3], Some(cbor(&room(&["older"])))),
    ];
    let probe = Probe::new(
        owner().verifying_key(),
        all.clone(),
        ChatRoomStateV1::default(),
        Policy::Newest,
    );
    let (found, read) = run(probe, &states);
    let Found::State(state, _) = found else {
        panic!("found nothing");
    };
    assert_eq!(texts(&state), vec!["newer"]);
    assert_eq!(read, all[..3].to_vec());
}

#[test]
fn a_key_that_does_not_answer_is_passed_over() {
    let all = candidates_down_to(&owner().verifying_key(), None);
    let states = vec![(all[0], None), (all[1], Some(cbor(&room(&["older"]))))];
    let probe = Probe::new(
        owner().verifying_key(),
        all.clone(),
        ChatRoomStateV1::default(),
        Policy::Newest,
    );
    let (found, _) = run(probe, &states);
    assert!(matches!(found, Found::State(s, _) if texts(&s) == vec!["older"]));
}

#[test]
fn every_key_answering_with_nothing_leaves_only_the_users_copy() {
    let all = candidates_down_to(&owner().verifying_key(), None);
    let probe = Probe::new(
        owner().verifying_key(),
        all.clone(),
        room(&["only in my copy"]),
        Policy::Fold,
    );
    let (found, read) = run(probe, &[]);
    let Found::Local(state) = found else {
        panic!("not the user's copy");
    };
    assert_eq!(texts(&state), vec!["only in my copy"]);
    assert_eq!(read, all);
}

#[test]
fn nothing_found_while_a_key_did_not_answer_is_nothing_yet() {
    let all = candidates_down_to(&owner().verifying_key(), None);
    let probe = Probe::new(
        owner().verifying_key(),
        all.clone(),
        room(&["mine"]),
        Policy::Fold,
    );
    let (found, _) = run(probe, &[(all[1], None)]);
    assert!(matches!(found, Found::Nothing));
}

#[test]
fn what_the_users_copy_holds_is_kept() {
    let all = candidates_down_to(&owner().verifying_key(), Some(&earlier()[0]));
    let states = vec![(all[0], Some(cbor(&room(&["from the network"]))))];
    let probe = Probe::new(
        owner().verifying_key(),
        all,
        room(&["only in my copy"]),
        Policy::Fold,
    );
    let (found, _) = run(probe, &states);
    let Found::State(state, unresolved) = found else {
        panic!("found nothing");
    };
    assert_eq!(texts(&state), vec!["from the network", "only in my copy"]);
    assert!(unresolved.is_empty());
}

/// A stored room is moved by folding EVERY earlier key down to the one it was stored under, as
/// the web UI's `start_rekey_merge` does: what reached only an older key is not lost.
#[test]
fn moving_a_stored_room_folds_every_earlier_key_down_to_its_own() {
    let all = candidates_down_to(&owner().verifying_key(), Some(&earlier()[2]));
    assert_eq!(all.len(), 3);
    let states = vec![
        (all[0], Some(cbor(&room(&["on the newest"])))),
        // One that does not answer is folded in later.
        (all[1], None),
        (all[2], Some(cbor(&room(&["on the stored key"])))),
    ];
    let probe = Probe::new(
        owner().verifying_key(),
        all.clone(),
        room(&[]),
        Policy::Fold,
    );
    let (found, read) = run(probe, &states);
    let Found::State(state, unresolved) = found else {
        panic!("found nothing");
    };
    assert_eq!(texts(&state), vec!["on the newest", "on the stored key"]);
    assert_eq!(read, all);
    assert_eq!(unresolved, vec![all[1]]);
}

#[test]
fn the_current_key_is_never_an_earlier_one() {
    let o = owner().verifying_key();
    let current = *create::contract_key(&o).id();
    assert!(candidates_down_to(&o, None).iter().all(|id| *id != current));
    assert!(
        legacy_contract_keys_for_owner(&o)
            .iter()
            .all(|k| *k.id() != current),
        "the current room contract is listed in common/legacy_room_contracts.toml"
    );
}

#[test]
fn the_put_goes_to_the_current_key_without_an_upgrade_pointer() {
    let o = owner();
    let mut state = room(&["hi"]);
    state.upgrade = OptionalUpgradeV1(Some(AuthorizedUpgradeV1::new(
        UpgradeV1 {
            owner_member_id: MemberId::from(&o.verifying_key()),
            version: 1,
            new_chatroom_address: blake3::hash(b"elsewhere"),
        },
        &o,
    )));
    let ContractRequest::Put {
        contract,
        state: sent,
        subscribe,
        ..
    } = put_forward(&o.verifying_key(), &state).unwrap()
    else {
        panic!("not a PUT");
    };
    assert_eq!(contract.key(), create::contract_key(&o.verifying_key()));
    assert!(!subscribe);
    let sent: ChatRoomStateV1 = ciborium::de::from_reader(sent.as_ref()).unwrap();
    assert!(sent.upgrade.0.is_none());
    assert_eq!(texts(&sent), vec!["hi"]);
    // What the room contract accepts: a state its cleanup leaves unchanged.
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    assert_eq!(cbor(&sent.normalized(&params).unwrap()), cbor(&sent));
}

#[test]
fn the_keys_a_slot_still_owes_a_merge_from_are_read_from_its_rekey_merge() {
    let o = owner().verifying_key();
    let owed = vec![earlier()[2], earlier()[4]];
    let slot = cbor(&ciborium::Value::Map(vec![(
        ciborium::Value::Text("Present".into()),
        ciborium::Value::Map(vec![
            (
                ciborium::Value::Text("owner_vk".into()),
                ciborium::Value::serialized(&o).unwrap(),
            ),
            (
                ciborium::Value::Text("rekey_merge".into()),
                ciborium::Value::Map(vec![(
                    ciborium::Value::Text("owed".into()),
                    ciborium::Value::serialized(&owed).unwrap(),
                )]),
            ),
        ]),
    )]));
    assert_eq!(owed_keys(&slot), owed);
    assert!(owed_keys(b"garbage").is_empty());
    // Down to the deepest of the stored key and what it owes.
    let all = candidates_down_to(&o, None);
    assert_eq!(
        candidates_down_to_owed(&o, &[earlier()[0], earlier()[4]]),
        all[..5].to_vec()
    );
    assert_eq!(
        candidates_down_to_owed(&o, &[earlier()[1]]),
        all[..2].to_vec()
    );
}
