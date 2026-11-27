use super::*;
use ed25519_dalek::SigningKey;

fn vk(seed: u8) -> VerifyingKey {
    SigningKey::from_bytes(&[seed; 32]).verifying_key()
}

fn cbor<T: Serialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).unwrap();
    out
}

/// The web UI's `RoomsMeta` (`ui/src/room_data.rs`), field for field with the same serde
/// attributes, so the bytes below are what the web UI's save writes.
#[derive(Clone, Serialize, Deserialize, Default, PartialEq, Debug)]
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
        identity_sources: vec![(vk(3), [9; 32])],
    }
}

#[test]
fn the_web_uis_bytes_read_back() {
    let modes = decode_modes(&cbor(&web_meta()));
    assert_eq!(modes.len(), 2);
    assert_eq!(modes[&vk(1)], NotificationMode::Muted);
    assert_eq!(modes[&vk(2)], NotificationMode::MentionsAndReplies);
    assert_eq!(modes.get(&vk(3)), None);
}

#[test]
fn a_mode_is_a_text_string_of_its_variant_name() {
    // What the web UI's derive writes, as literal CBOR: a text string of the variant name.
    assert_eq!(cbor(&NotificationMode::All), [&[0x63][..], b"All"].concat());
    assert_eq!(
        cbor(&NotificationMode::MentionsAndReplies),
        [&[0x72][..], b"MentionsAndReplies"].concat()
    );
    assert_eq!(
        cbor(&NotificationMode::Muted),
        [&[0x65][..], b"Muted"].concat()
    );
}

/// The headline interop check: the value this app writes decodes with the web UI's type to the
/// web UI's own value plus the one change, with `room_order`, the open room and the identity
/// sources untouched.
#[test]
fn a_write_decodes_in_the_web_ui_with_everything_else_kept() {
    let before = web_meta();
    let out = meta_with_mode(Some(&cbor(&before)), &vk(3), NotificationMode::Muted)
        .unwrap()
        .expect("a change");
    let after: WebRoomsMeta = ciborium::de::from_reader(out.as_slice()).unwrap();
    let mut want = before.clone();
    want.notification_modes
        .insert(vk(3), NotificationMode::Muted);
    assert_eq!(after, want);
    assert_eq!(after.room_order, before.room_order);
    assert_eq!(after.current_room_key, before.current_room_key);
    assert_eq!(after.identity_sources, before.identity_sources);

    // Changing a room that already has an entry replaces just that entry.
    let out = meta_with_mode(Some(&cbor(&before)), &vk(1), NotificationMode::All)
        .unwrap()
        .unwrap();
    let after: WebRoomsMeta = ciborium::de::from_reader(out.as_slice()).unwrap();
    assert_eq!(after.notification_modes[&vk(1)], NotificationMode::All);
    assert_eq!(
        after.notification_modes[&vk(2)],
        NotificationMode::MentionsAndReplies
    );
    assert_eq!(after.room_order, before.room_order);
}

#[test]
fn the_same_mode_writes_nothing() {
    let bytes = cbor(&web_meta());
    assert_eq!(
        meta_with_mode(Some(&bytes), &vk(1), NotificationMode::Muted),
        Ok(None)
    );
}

#[test]
fn unknown_fields_and_unknown_modes_survive() {
    #[derive(Serialize)]
    struct Future {
        room_order: Vec<VerifyingKey>,
        notification_modes: HashMap<VerifyingKey, String>,
        something_new: (u32, String),
    }
    let before = cbor(&Future {
        room_order: vec![vk(2), vk(1)],
        // A mode a later web UI added: this build can't name it, and must not lose it.
        notification_modes: HashMap::from([(vk(2), "DigestOnly".to_owned())]),
        something_new: (7, "x".into()),
    });
    let out = meta_with_mode(Some(&before), &vk(1), NotificationMode::Muted)
        .unwrap()
        .unwrap();
    let value: Value = ciborium::de::from_reader(out.as_slice()).unwrap();
    let Value::Map(entries) = value else { panic!() };
    let get = |name: &str| {
        entries
            .iter()
            .find(|(k, _)| k.as_text() == Some(name))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        get("something_new"),
        Some(Value::serialized(&(7u32, "x")).unwrap())
    );
    assert_eq!(
        get("room_order"),
        Some(Value::serialized(&vec![vk(2), vk(1)]).unwrap())
    );
    let Some(Value::Map(modes)) = get("notification_modes") else {
        panic!()
    };
    assert_eq!(modes.len(), 2);
    assert!(modes.iter().any(
        |(k, v)| k.deserialized::<VerifyingKey>().ok() == Some(vk(2))
            && v.as_text() == Some("DigestOnly")
    ));
    // The unreadable mode reads as nothing here (so the room shows as All), not as an error.
    let read = decode_modes(&out);
    assert_eq!(read.get(&vk(2)), None);
    assert_eq!(read[&vk(1)], NotificationMode::Muted);
}

#[test]
fn an_empty_key_gets_a_value_the_web_ui_decodes() {
    for current in [None, Some(&[][..])] {
        let out = meta_with_mode(current, &vk(5), NotificationMode::MentionsAndReplies)
            .unwrap()
            .unwrap();
        let web: WebRoomsMeta = ciborium::de::from_reader(out.as_slice()).unwrap();
        assert_eq!(
            web.notification_modes,
            HashMap::from([(vk(5), NotificationMode::MentionsAndReplies)])
        );
        assert!(web.room_order.is_empty());
    }
}

#[test]
fn a_meta_without_modes_gains_the_field_and_keeps_the_rest() {
    #[derive(Serialize)]
    struct Old {
        current_room_key: Option<VerifyingKey>,
        room_order: Vec<VerifyingKey>,
    }
    let before = cbor(&Old {
        current_room_key: Some(vk(2)),
        room_order: vec![vk(2), vk(1)],
    });
    let out = meta_with_mode(Some(&before), &vk(1), NotificationMode::Muted)
        .unwrap()
        .unwrap();
    let web: WebRoomsMeta = ciborium::de::from_reader(out.as_slice()).unwrap();
    assert_eq!(web.current_room_key, Some(vk(2)));
    assert_eq!(web.room_order, vec![vk(2), vk(1)]);
    assert_eq!(web.notification_modes[&vk(1)], NotificationMode::Muted);
}

#[test]
fn what_is_not_understood_is_an_error_and_never_overwritten() {
    assert!(meta_with_mode(Some(&cbor(&"text")), &vk(1), NotificationMode::Muted).is_err());
    assert!(meta_with_mode(Some(&[0xff, 0xff]), &vk(1), NotificationMode::Muted).is_err());
    #[derive(Serialize)]
    struct Odd {
        notification_modes: u32,
    }
    assert!(
        meta_with_mode(
            Some(&cbor(&Odd {
                notification_modes: 3
            })),
            &vk(1),
            NotificationMode::Muted
        )
        .is_err()
    );
}

#[test]
fn unreadable_values_read_as_no_modes() {
    assert!(decode_modes(&[]).is_empty());
    assert!(decode_modes(&[0xff]).is_empty());
    assert!(decode_modes(&cbor(&"x")).is_empty());
}

#[test]
fn the_web_uis_labels_and_help() {
    // `MODES` in `ui/src/components/room_list/notification_modal.rs`.
    let got: Vec<_> = NotificationMode::ALL
        .iter()
        .map(|m| (m.label(), m.help()))
        .collect();
    assert_eq!(
        got,
        [
            ("All messages", "Notify for every message in this room."),
            (
                "Mentions & replies only",
                "Notify only when someone @mentions you or replies to your message."
            ),
            ("Muted", "Never notify for this room."),
        ]
    );
    assert_eq!(
        NotificationMode::default(),
        NotificationMode::MentionsAndReplies
    );
}
