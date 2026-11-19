use super::*;
use crate::rooms::modes::NotificationMode;
use ed25519_dalek::SigningKey;
use std::collections::HashMap;

fn vk(seed: u8) -> VerifyingKey {
    SigningKey::from_bytes(&[seed; 32]).verifying_key()
}

fn cbor<T: Serialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).unwrap();
    out
}

/// The web UI's `RoomsMeta` (`ui/src/room_data.rs`), field for field with the same serde
/// attributes, so the bytes are what the web UI's save writes.
#[derive(Clone, Serialize, serde::Deserialize, Default, PartialEq, Debug)]
struct WebRoomsMeta {
    #[serde(default)]
    current_room_key: Option<VerifyingKey>,
    #[serde(default)]
    notification_modes: HashMap<VerifyingKey, NotificationMode>,
    #[serde(default)]
    room_order: Vec<VerifyingKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    identity_sources: Vec<(VerifyingKey, [u8; 32])>,
}

fn web_meta() -> WebRoomsMeta {
    WebRoomsMeta {
        current_room_key: Some(vk(1)),
        notification_modes: HashMap::from([
            (vk(1), NotificationMode::Muted),
            (vk(2), NotificationMode::MentionsAndReplies),
        ]),
        room_order: vec![vk(2), vk(1), vk(3)],
        identity_sources: vec![(vk(1), [8; 32]), (vk(3), [9; 32])],
    }
}

fn read_meta(bytes: &[u8]) -> WebRoomsMeta {
    ciborium::de::from_reader(bytes).expect("the web UI reads it")
}

#[test]
fn the_tombstone_is_the_web_uis_unit_variant() {
    // `RoomSlot::Tombstone` as ciborium writes it: the text string "Tombstone".
    let written = tombstone_write(None).unwrap().unwrap();
    assert_eq!(written, [&[0x69][..], b"Tombstone"].concat());
    assert!(matches!(
        decode_room_slot(&written),
        Ok(StoredRoomSlot::Tombstone)
    ));
}

#[test]
fn a_tombstone_replaces_a_room_and_anything_unreadable_but_not_a_tombstone() {
    let tomb = tombstone_write(None).unwrap().unwrap();
    // Already a tombstone: nothing to write.
    assert_eq!(tombstone_write(Some(&tomb)).unwrap(), None);
    // A present room (here a stand-in the reader cannot decode, and an empty value) and bytes
    // that are not CBOR at all: all replaced, as the web UI's `reconcile_room_tombstone` does.
    for current in [&cbor(&"Present")[..], &[][..], &[0xff, 0x00][..]] {
        assert_eq!(
            tombstone_write(Some(current)).unwrap().as_deref(),
            Some(&tomb[..])
        );
    }
}

#[test]
fn leaving_takes_the_room_out_of_the_order_the_open_room_and_the_identity_sources() {
    let out = meta_without_room(Some(&cbor(&web_meta())), &vk(1))
        .unwrap()
        .expect("changed");
    let read = read_meta(&out);
    assert_eq!(read.room_order, vec![vk(2), vk(3)]);
    assert_eq!(read.current_room_key, None);
    assert_eq!(read.identity_sources, vec![(vk(3), [9; 32])]);
    // The notification modes stay, the left room's included (the web UI keeps it).
    assert_eq!(read.notification_modes, web_meta().notification_modes);
}

#[test]
fn another_room_staying_open_is_left_open() {
    let out = meta_without_room(Some(&cbor(&web_meta())), &vk(3))
        .unwrap()
        .expect("changed");
    let read = read_meta(&out);
    assert_eq!(read.current_room_key, Some(vk(1)));
    assert_eq!(read.room_order, vec![vk(2), vk(1)]);
}

#[test]
fn the_last_identity_source_leaves_the_field_out_as_the_web_ui_writes_it() {
    let mut meta = web_meta();
    meta.identity_sources = vec![(vk(1), [8; 32])];
    let out = meta_without_room(Some(&cbor(&meta)), &vk(1))
        .unwrap()
        .expect("changed");
    meta.identity_sources.clear();
    meta.current_room_key = None;
    meta.room_order = vec![vk(2), vk(3)];
    // Byte for byte what the web UI's own derive writes for that value.
    let Value::Map(entries) = ciborium::de::from_reader::<Value, _>(&out[..]).unwrap() else {
        panic!("a map")
    };
    assert!(
        entries
            .iter()
            .all(|(k, _)| k.as_text() != Some("identity_sources"))
    );
    assert_eq!(read_meta(&out), meta);
}

#[test]
fn fields_this_app_does_not_know_are_kept_exactly() {
    #[derive(Serialize)]
    struct Future {
        room_order: Vec<VerifyingKey>,
        current_room_key: Option<VerifyingKey>,
        from_a_newer_build: BTreeMapLike,
    }
    #[derive(Serialize)]
    struct BTreeMapLike {
        n: u32,
        words: Vec<String>,
    }
    let stored = cbor(&Future {
        room_order: vec![vk(1), vk(2)],
        current_room_key: Some(vk(2)),
        from_a_newer_build: BTreeMapLike {
            n: 7,
            words: vec!["a".into(), "b".into()],
        },
    });
    let out = meta_without_room(Some(&stored), &vk(1)).unwrap().unwrap();
    let Value::Map(entries) = ciborium::de::from_reader::<Value, _>(&out[..]).unwrap() else {
        panic!("a map")
    };
    let Value::Map(before) = ciborium::de::from_reader::<Value, _>(&stored[..]).unwrap() else {
        panic!("a map")
    };
    let keep = |e: &[(Value, Value)]| {
        e.iter()
            .find(|(k, _)| k.as_text() == Some("from_a_newer_build"))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(keep(&entries), keep(&before));
    assert!(keep(&entries).is_some());
    assert_eq!(read_meta(&out).room_order, vec![vk(2)]);
}

#[test]
fn nothing_naming_the_room_writes_nothing() {
    assert_eq!(
        meta_without_room(Some(&cbor(&web_meta())), &vk(9)),
        Ok(None)
    );
    assert_eq!(meta_without_room(None, &vk(1)), Ok(None));
    assert_eq!(meta_without_room(Some(&[]), &vk(1)), Ok(None));
    // Run twice, the second finds nothing to do.
    let once = meta_without_room(Some(&cbor(&web_meta())), &vk(1))
        .unwrap()
        .unwrap();
    assert_eq!(meta_without_room(Some(&once), &vk(1)), Ok(None));
}

#[test]
fn a_value_the_web_ui_never_wrote_is_an_error_not_overwritten() {
    assert!(meta_without_room(Some(&cbor(&"nope")), &vk(1)).is_err());
    assert!(meta_without_room(Some(&[0xff, 0x00]), &vk(1)).is_err());
    #[derive(Serialize)]
    struct Odd {
        room_order: u32,
    }
    assert!(meta_without_room(Some(&cbor(&Odd { room_order: 1 })), &vk(1)).is_err());
}

/// The web UI's own source, which the real types cannot be linked from here: the names this
/// module edits by, and the tombstone variant, are still declared as it expects. (The `ui`
/// crate's `leave_is_stored_as_the_native_app_writes_it` pins the bytes with the real types.)
#[test]
fn the_web_uis_declarations_are_the_ones_leave_edits_by() {
    // A Windows checkout may have CRLF line endings.
    let src = include_str!("../../../../ui/src/room_data.rs").replace("\r\n", "\n");
    let meta = src
        .split("pub struct RoomsMeta {")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("RoomsMeta is declared");
    for field in [
        "pub current_room_key: Option<VerifyingKey>",
        "pub room_order: Vec<VerifyingKey>",
        "pub identity_sources: Vec<(VerifyingKey, [u8; 32])>",
        "pub notification_modes: HashMap<VerifyingKey, NotificationMode>",
    ] {
        assert!(meta.contains(field), "RoomsMeta lost `{field}`");
    }
    assert!(
        meta.contains(
            "#[serde(default, skip_serializing_if = \"Vec::is_empty\")]\n    pub identity_sources"
        ),
        "identity_sources is no longer left out when empty"
    );
    let slot = src
        .split("pub enum RoomSlot {")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("RoomSlot is declared");
    assert!(slot.contains("Present(Box<RoomData>)"));
    assert!(
        slot.contains("\n    Tombstone,"),
        "RoomSlot::Tombstone changed"
    );
}
