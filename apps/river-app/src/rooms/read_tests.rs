use super::*;
use crate::rooms::create::new_room;
use ed25519_dalek::SigningKey;
use river_core::room_state::member::MemberId;
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::stored_rooms::encode_new_room;
use std::time::{Duration, SystemTime};

fn owner() -> SigningKey {
    SigningKey::from_bytes(&[5; 32])
}

fn message(text: &str, secs_ago: u64) -> AuthorizedMessageV1 {
    let owner = owner();
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: MemberId::from(&owner.verifying_key()),
            author: MemberId::from(&owner.verifying_key()),
            time: (SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 - secs_ago)).into(),
            content: RoomMessageBody::public(text.into()),
        },
        &owner,
    )
}

/// Four messages, oldest first.
fn messages() -> Vec<AuthorizedMessageV1> {
    (0..4)
        .map(|i| message(&format!("m{i}"), 100 - i * 10))
        .collect()
}

fn keys(ms: &[AuthorizedMessageV1]) -> Known {
    ms.iter().map(|m| m.order_key()).collect()
}

/// A slot as the web UI stores it, holding `stored` messages and the marker `at`, with a field
/// this app does not know.
fn slot(stored: &[AuthorizedMessageV1], at: Option<&AuthorizedMessageV1>) -> Vec<u8> {
    let mut plan = new_room(owner(), "Book club", "Ada", false).unwrap();
    plan.room.room_state.recent_messages.messages = stored.to_vec();
    plan.room.last_read_message_id = at.map(|m| m.id());
    let bytes = encode_new_room(&plan.room).unwrap();
    let Value::Map(mut outer) = ciborium::de::from_reader::<Value, _>(&bytes[..]).unwrap() else {
        panic!("a map");
    };
    let Some(Value::Map(fields)) = outer
        .iter_mut()
        .find(|(k, _)| k.as_text() == Some("Present"))
        .map(|(_, v)| v)
    else {
        panic!("present");
    };
    fields.push((Value::Text("from_the_future".into()), Value::from(7u32)));
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(outer), &mut out).unwrap();
    out
}

fn marker(bytes: &[u8]) -> Option<MessageId> {
    let Ok(StoredRoomSlot::Present(r)) = decode_room_slot(bytes) else {
        panic!("present");
    };
    r.last_read_message_id
}

fn field(bytes: &[u8], name: &str) -> Option<Value> {
    let Value::Map(outer) = ciborium::de::from_reader::<Value, _>(bytes).unwrap() else {
        panic!("a map");
    };
    let (_, Value::Map(fields)) = outer
        .into_iter()
        .find(|(k, _)| k.as_text() == Some("Present"))?
    else {
        return None;
    };
    fields
        .into_iter()
        .find(|(k, _)| k.as_text() == Some(name))
        .map(|(_, v)| v)
}

#[test]
fn a_marker_moves_forward_and_only_it_changes() {
    let ms = messages();
    let before = slot(&ms, Some(&ms[1]));
    let after = slot_with_marker(Some(&before), &ms[3].order_key(), &keys(&ms), None)
        .unwrap()
        .expect("a change is written");
    assert_eq!(marker(&after), Some(ms[3].id()));
    // The web UI reads the id the way it writes its own.
    assert_eq!(
        field(&after, MARKER_FIELD),
        Some(Value::serialized(&Some(ms[3].id())).unwrap())
    );
    // The rest of the slot is as the web UI left it, down to a field this app does not know.
    assert_eq!(
        field(&after, "from_the_future"),
        Some(Value::from(7u32)),
        "unknown field kept"
    );
    for name in [
        "owner_vk",
        "room_state",
        "self_sk",
        "contract_key",
        "self_nickname",
    ] {
        assert_eq!(field(&after, name), field(&before, name), "{name}");
    }
}

#[test]
fn a_slot_with_no_marker_gets_one() {
    let ms = messages();
    let before = slot(&ms, None);
    let after = slot_with_marker(Some(&before), &ms[0].order_key(), &keys(&ms), None)
        .unwrap()
        .unwrap();
    assert_eq!(marker(&after), Some(ms[0].id()));
}

#[test]
fn a_marker_never_moves_back_or_is_written_twice() {
    let ms = messages();
    let before = slot(&ms, Some(&ms[2]));
    // Behind, or the same one: nothing to store.
    assert_eq!(
        slot_with_marker(Some(&before), &ms[1].order_key(), &keys(&ms), None),
        Ok(None)
    );
    assert_eq!(
        slot_with_marker(Some(&before), &ms[2].order_key(), &keys(&ms), None),
        Ok(None)
    );
}

#[test]
fn a_marker_the_web_ui_stored_past_what_this_app_holds_is_not_moved_back() {
    // The web UI has seen the fourth message and stored it, with its own copy of the room.
    // This app holds only the first three, and read them all.
    let ms = messages();
    let before = slot(&ms, Some(&ms[3]));
    assert_eq!(
        slot_with_marker(Some(&before), &ms[2].order_key(), &keys(&ms[..3]), None),
        Ok(None),
        "found in the slot's own copy, and further"
    );
}

#[test]
fn a_stored_marker_found_nowhere_loses_to_one_that_is() {
    // The web UI's rule: a marker that is not in the messages (evicted) places nothing.
    let ms = messages();
    let gone = message("gone", 5000);
    let before = slot(&ms, Some(&gone));
    let after = slot_with_marker(Some(&before), &ms[1].order_key(), &keys(&ms), None)
        .unwrap()
        .expect("written");
    assert_eq!(marker(&after), Some(ms[1].id()));
}

#[test]
fn a_room_that_is_not_stored_or_was_left_is_not_written() {
    let ms = messages();
    assert_eq!(
        slot_with_marker(None, &ms[0].order_key(), &keys(&ms), None),
        Ok(None)
    );
    let mut tomb = Vec::new();
    ciborium::ser::into_writer(&"Tombstone", &mut tomb).unwrap();
    assert_eq!(
        slot_with_marker(Some(&tomb), &ms[0].order_key(), &keys(&ms), None),
        Ok(None)
    );
}

#[test]
fn a_slot_this_app_cannot_read_is_an_error_and_left_alone() {
    let ms = messages();
    assert!(slot_with_marker(Some(b"\xff\xff"), &ms[0].order_key(), &keys(&ms), None).is_err());
}

#[test]
fn further_means_later_in_the_room_and_found() {
    let ms = messages();
    let k = keys(&ms);
    let id = |i: usize| ms[i].id();
    assert!(is_further(&id(2), Some(&id(1)), &k, &[]));
    assert!(!is_further(&id(1), Some(&id(2)), &k, &[]));
    assert!(!is_further(&id(1), Some(&id(1)), &k, &[]));
    assert!(is_further(&id(0), None, &k, &[]));
    let gone = message("gone", 9000).id();
    // Found beats not found, either side; neither found keeps the held one.
    assert!(is_further(&id(0), Some(&gone), &k, &[]));
    assert!(!is_further(&gone, Some(&id(0)), &k, &[]));
    assert!(!is_further(&gone, Some(&gone), &k, &[]));
}

/// The web UI's count of unread messages after the marker (`unread_candidate_messages` in
/// `ui/src/components/app/document_title.rs`): from the message after the marker in the slot's
/// own copy of the room, or all of them when the marker is not in it.
fn web_unread(bytes: &[u8]) -> usize {
    let Ok(StoredRoomSlot::Present(r)) = decode_room_slot(bytes) else {
        panic!("present");
    };
    let recent = &r.room_state.recent_messages.messages;
    let start = match r.last_read_message_id.as_ref() {
        None => 0,
        Some(id) => recent
            .iter()
            .position(|m| &m.id() == id)
            .map_or(0, |i| i + 1),
    };
    recent[start..].len()
}

#[test]
fn a_marker_newer_than_the_slots_copy_of_the_room_is_stored_with_the_message_it_names() {
    // The web UI stored the first two messages; this app read all four.
    let ms = messages();
    let before = slot(&ms[..2], Some(&ms[0]));
    let mut native = new_room(owner(), "Book club", "Ada", false)
        .unwrap()
        .room
        .room_state;
    native.recent_messages.messages = ms.clone();
    let after = slot_with_marker(Some(&before), &ms[3].order_key(), &keys(&ms), Some(&native))
        .unwrap()
        .expect("written");
    assert_eq!(marker(&after), Some(ms[3].id()));
    assert_eq!(
        web_unread(&after),
        0,
        "the web UI finds the marker and counts nothing after the newest message"
    );
    // Without the merge the web UI would have counted every message.
    let bare = slot_with_marker(Some(&before), &ms[3].order_key(), &keys(&ms), None);
    let bare = bare
        .unwrap()
        .expect("a marker the slot holds is still moved");
    assert_eq!(marker(&bare), Some(ms[1].id()));
    assert_eq!(web_unread(&bare), 0, "never worse than before");
    assert_eq!(web_unread(&before), 1);
    // The rest of the slot is untouched.
    assert_eq!(field(&after, "from_the_future"), Some(Value::from(7u32)));
    assert_eq!(field(&after, "owner_vk"), field(&before, "owner_vk"));
}

#[test]
fn a_marker_the_slot_cannot_locate_and_no_merge_can_place_writes_nothing() {
    // None of what this app holds is in the slot's copy, and the marker is not either.
    let ms = messages();
    let before = slot(&ms[..1], None);
    let later = &ms[2..];
    assert_eq!(
        slot_with_marker(Some(&before), &later[1].order_key(), &keys(later), None),
        Ok(None)
    );
}
