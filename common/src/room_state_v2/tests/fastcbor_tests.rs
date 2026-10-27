//! The v1 codec against ciborium: on every legacy record kind, both encode
//! to the same bytes and both decode the same bytes to the same value, and
//! the codec refuses the non-canonical forms ciborium's re-encode check
//! refuses.

use crate::room_state::ban::AuthorizedUserBan;
use crate::room_state::configuration::AuthorizedConfigurationV1;
use crate::room_state::direct_messages::{AuthorizedDirectMessage, AuthorizedRecipientPurges};
use crate::room_state::member::AuthorizedMember;
use crate::room_state::member_info::AuthorizedMemberInfo;
use crate::room_state::message::AuthorizedMessageV1;
use crate::room_state::secret::AuthorizedEncryptedSecretForMember;
use crate::room_state_v2::bench::{largest_state, Caps};
use crate::room_state_v2::encoding::{from_cbor_canonical, to_cbor};
use crate::room_state_v2::fastcbor;
use crate::room_state_v2::records::Record;
use serde::{de::DeserializeOwned, Serialize};

fn same<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(bytes: &[u8]) {
    let c: T = from_cbor_canonical(bytes).expect("ciborium decodes the record");
    assert_eq!(fastcbor::to_vec(&c), bytes, "encodings differ");
    let f: T = fastcbor::from_canonical(bytes).expect("the codec decodes the record");
    assert_eq!(f, c, "decoded values differ");
    // Non-canonical forms: a trailing byte, and the first header widened.
    let mut t = bytes.to_vec();
    t.push(0);
    assert!(fastcbor::from_canonical::<T>(&t).is_none());
    assert!(from_cbor_canonical::<T>(&t).is_none());
    if bytes[0] & 31 < 24 {
        let mut w = vec![(bytes[0] & 0xe0) | 24, bytes[0] & 31];
        w.extend_from_slice(&bytes[1..]);
        assert!(fastcbor::from_canonical::<T>(&w).is_none());
        assert!(from_cbor_canonical::<T>(&w).is_none());
    }
}

/// Checks one legacy record's v1 bytes; returns its kind.
pub fn check_legacy(r: &Record) -> Option<u8> {
    match r {
        Record::LegacyConfiguration(l) => same::<AuthorizedConfigurationV1>(&l.v1.0),
        Record::LegacyLink(l) => same::<AuthorizedMember>(&l.v1.0),
        Record::LegacyBan(l) => same::<AuthorizedUserBan>(&l.v1.0),
        Record::LegacyActivity(l) | Record::LegacyMessage(l) => {
            same::<AuthorizedMessageV1>(&l.v1.0)
        }
        Record::LegacyMemberInfo(l) => same::<AuthorizedMemberInfo>(&l.v1.0),
        Record::LegacyDirectMessage(l) => same::<AuthorizedDirectMessage>(&l.v1.0),
        Record::LegacyPurge(l) => same::<AuthorizedRecipientPurges>(&l.v1.0),
        Record::LegacySecretBlob(l) => same::<AuthorizedEncryptedSecretForMember>(&l.v1.0),
        _ => return None,
    }
    Some(r.kind())
}

#[test]
fn fastcbor_matches_ciborium_on_generated_v1_records() {
    let mut seen = std::collections::BTreeSet::new();
    // Every record the builder makes is kept (dropped == 0), so the loop
    // below sees every kind, including any the codec might fail to decode.
    let (s, dropped) = largest_state(Caps::ceilings_legacy().scaled(4), 0);
    assert_eq!(dropped, 0);
    for r in &s.records {
        if let Some(k) = check_legacy(r) {
            seen.insert(k);
        }
    }
    // The private-room variant carries sealed nicknames, private messages
    // and blobs.
    let (s, dropped) = largest_state(
        Caps {
            max_secret_versions: 2,
            ..Caps::ceilings_legacy().scaled(4)
        },
        0,
    );
    assert_eq!(dropped, 0);
    for r in &s.records {
        if let Some(k) = check_legacy(r) {
            seen.insert(k);
        }
    }
    let v1 = super::convert_tests::v1_room();
    let o = crate::room_state_v2::encoding::Key::from(&super::convert_tests::owner_vk());
    for r in crate::room_state_v2::convert::legacy_records(&o, &v1).records {
        check_legacy(&r);
    }
    check_legacy(&crate::room_state_v2::convert::legacy_configuration(
        &o, &v1,
    ));
    let _ = to_cbor(&0u8);
    // Every legacy kind but the configuration (checked just above).
    assert_eq!(
        seen.into_iter().collect::<Vec<_>>(),
        vec![8, 10, 12, 15, 17, 19, 21, 23]
    );
}

/// The same on a fetched live v1 state (RIVER_V1_STATE, RIVER_V1_OWNER).
#[test]
#[ignore = "reads RIVER_V1_STATE"]
fn fastcbor_matches_ciborium_on_a_live_room() {
    use crate::room_state::ChatRoomStateV1;
    let v1: ChatRoomStateV1 = ciborium::de::from_reader(
        &std::fs::read(std::env::var("RIVER_V1_STATE").unwrap()).unwrap()[..],
    )
    .unwrap();
    let b = bs58::decode(std::env::var("RIVER_V1_OWNER").unwrap())
        .into_vec()
        .unwrap();
    let o = crate::room_state_v2::encoding::Bytes32(b.try_into().unwrap());
    let recs = crate::room_state_v2::convert::legacy_records(&o, &v1).records;
    for r in &recs {
        check_legacy(r);
    }
    check_legacy(&crate::room_state_v2::convert::legacy_configuration(
        &o, &v1,
    ));
    println!(
        "{} live v1 records encode and decode identically",
        recs.len() + 1
    );
}
