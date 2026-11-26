use super::*;
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn vk(seed: u8) -> VerifyingKey {
    SigningKey::from_bytes(&[seed; 32]).verifying_key()
}

fn cbor<T: Serialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).unwrap();
    out
}

/// The web UI's `RoomsMeta` (`ui/src/room_data.rs`) with the same serde attributes, plus a field
/// a newer web UI might add, so the bytes below are what the web UI's save writes.
#[derive(Clone, Serialize, Deserialize, Default, PartialEq, Debug)]
struct WebRoomsMeta {
    #[serde(default)]
    current_room_key: Option<VerifyingKey>,
    #[serde(default)]
    notification_modes: HashMap<VerifyingKey, String>,
    #[serde(default)]
    room_order: Vec<VerifyingKey>,
    #[serde(default)]
    from_the_future: Option<u32>,
}

fn web_meta(order: &[VerifyingKey]) -> WebRoomsMeta {
    WebRoomsMeta {
        current_room_key: Some(vk(1)),
        notification_modes: HashMap::from([(vk(2), "Muted".to_owned())]),
        room_order: order.to_vec(),
        from_the_future: Some(7),
    }
}

fn read_back(bytes: &[u8]) -> WebRoomsMeta {
    ciborium::de::from_reader(bytes).unwrap()
}

#[test]
fn the_web_uis_bytes_read_back() {
    assert_eq!(
        decode_order(&cbor(&web_meta(&[vk(3), vk(1)]))),
        vec![vk(3), vk(1)]
    );
    assert!(decode_order(b"junk").is_empty());
}

#[test]
fn the_new_order_is_stored_and_everything_else_is_kept() {
    let stored = cbor(&web_meta(&[vk(1), vk(2), vk(3)]));
    let written = meta_with_order(Some(&stored), &[vk(2), vk(1), vk(3)])
        .unwrap()
        .expect("a change is written");
    let mut want = web_meta(&[vk(2), vk(1), vk(3)]);
    assert_eq!(read_back(&written), want);
    // The web UI reads it as it reads its own save.
    want.room_order = decode_order(&written);
    assert_eq!(want.room_order, vec![vk(2), vk(1), vk(3)]);
}

#[test]
fn the_order_shown_is_written_whole_even_for_rooms_the_store_never_placed() {
    // The web UI's drag stores the whole visible order, not only the rooms the user placed.
    let stored = cbor(&web_meta(&[vk(1)]));
    let written = meta_with_order(Some(&stored), &[vk(2), vk(1), vk(3)])
        .unwrap()
        .unwrap();
    assert_eq!(decode_order(&written), vec![vk(2), vk(1), vk(3)]);
}

#[test]
fn rooms_in_the_store_that_are_not_shown_keep_their_place_beside_their_neighbours() {
    // The web UI has a room (9) this app has not read yet, between 1 and 2. Moving 2 before 1
    // keeps 9 next to 1, as the web UI's `set_visible_order` does (#802).
    let stored = cbor(&web_meta(&[vk(1), vk(9), vk(2)]));
    let written = meta_with_order(Some(&stored), &[vk(2), vk(1)])
        .unwrap()
        .unwrap();
    assert_eq!(decode_order(&written), vec![vk(2), vk(1), vk(9)]);
}

#[test]
fn an_order_the_store_already_holds_is_not_written() {
    let stored = cbor(&web_meta(&[vk(2), vk(1)]));
    assert_eq!(meta_with_order(Some(&stored), &[vk(2), vk(1)]), Ok(None));
}

#[test]
fn nothing_stored_gets_a_map_holding_only_the_order() {
    for current in [None, Some(&[][..])] {
        let written = meta_with_order(current, &[vk(2), vk(1)]).unwrap().unwrap();
        assert_eq!(
            read_back(&written),
            WebRoomsMeta {
                room_order: vec![vk(2), vk(1)],
                ..Default::default()
            }
        );
    }
}

#[test]
fn a_field_missing_from_the_store_is_added_beside_the_others() {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(
        &Value::Map(vec![(
            Value::Text("current_room_key".into()),
            Value::serialized(&vk(1)).unwrap(),
        )]),
        &mut bytes,
    )
    .unwrap();
    let written = meta_with_order(Some(&bytes), &[vk(2), vk(1)])
        .unwrap()
        .unwrap();
    let meta = read_back(&written);
    assert_eq!(meta.room_order, vec![vk(2), vk(1)]);
    assert_eq!(meta.current_room_key, Some(vk(1)));
}

#[test]
fn a_value_this_app_cannot_read_is_an_error_and_never_overwritten() {
    assert!(meta_with_order(Some(&cbor(&"text")), &[vk(1)]).is_err());
    assert!(meta_with_order(Some(b"\xff\xff"), &[vk(1)]).is_err());
    let mut bad = Vec::new();
    ciborium::ser::into_writer(
        &Value::Map(vec![(Value::Text("room_order".into()), Value::Null)]),
        &mut bad,
    )
    .unwrap();
    assert!(meta_with_order(Some(&bad), &[vk(1)]).is_err());
}

#[test]
fn moving_a_room_before_another_or_to_the_end() {
    let shown = [vk(1), vk(2), vk(3), vk(4)];
    assert_eq!(
        move_before(&shown, &vk(3), Some(&vk(2))),
        vec![vk(1), vk(3), vk(2), vk(4)]
    );
    assert_eq!(
        move_before(&shown, &vk(1), Some(&vk(4))),
        vec![vk(2), vk(3), vk(1), vk(4)]
    );
    assert_eq!(
        move_before(&shown, &vk(1), None),
        vec![vk(2), vk(3), vk(4), vk(1)]
    );
    // Nothing to do: itself, an unknown room, an unknown target, or already there.
    assert_eq!(move_before(&shown, &vk(1), Some(&vk(1))), shown.to_vec());
    assert_eq!(move_before(&shown, &vk(9), Some(&vk(1))), shown.to_vec());
    assert_eq!(move_before(&shown, &vk(1), Some(&vk(9))), shown.to_vec());
    assert_eq!(move_before(&shown, &vk(4), None), shown.to_vec());
    assert_eq!(move_before(&shown, &vk(1), Some(&vk(2))), shown.to_vec());
}

#[test]
fn merging_places_a_stranger_next_to_the_neighbour_it_followed() {
    let mut base = vec![vk(1), vk(3)];
    merge_room_order(&mut base, &[vk(1), vk(2), vk(3), vk(4)]);
    assert_eq!(base, vec![vk(1), vk(2), vk(3), vk(4)]);
    // With nothing before it in common, it goes before the next one that is there.
    let mut base = vec![vk(3)];
    merge_room_order(&mut base, &[vk(2), vk(3)]);
    assert_eq!(base, vec![vk(2), vk(3)]);
    // Never a duplicate.
    let mut base = vec![vk(1)];
    merge_room_order(&mut base, &[vk(1), vk(1)]);
    assert_eq!(base, vec![vk(1)]);
}
