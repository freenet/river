//! The v1 codec against ciborium: on every legacy record kind, both encode
//! to the same bytes and both decode the same bytes to the same value, and
//! the codec refuses the non-canonical forms ciborium's re-encode check
//! refuses. And the signed bytes `legacy` takes as a span of the v1 bytes
//! are the bytes the ciborium path encoded and verified, so every signature
//! check gives the old answer, and a non-canonical record whose signed span
//! is intact is still refused.

use crate::room_state::ban::AuthorizedUserBan;
use crate::room_state::configuration::AuthorizedConfigurationV1;
use crate::room_state::direct_messages::{AuthorizedDirectMessage, AuthorizedRecipientPurges};
use crate::room_state::member::AuthorizedMember;
use crate::room_state::member_info::AuthorizedMemberInfo;
use crate::room_state::message::AuthorizedMessageV1;
use crate::room_state::secret::AuthorizedEncryptedSecretForMember;
use crate::room_state_v2::bench::{largest_state, Caps};
use crate::room_state_v2::encoding::{from_cbor_canonical, to_cbor, verify_bytes, Key, Sig};
use crate::room_state_v2::fastcbor;
use crate::room_state_v2::records::{Legacy, Record};
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

/// The entries of a top-level map of up to 23 entries with short text keys
/// (every v1 authorized struct): each entry's key-and-value byte range.
fn entries(bytes: &[u8]) -> Vec<std::ops::Range<usize>> {
    assert_eq!(bytes[0] >> 5, 5, "a map");
    let n = (bytes[0] & 31) as usize;
    assert!(n < 24);
    let mut out = Vec::new();
    let mut i = 1;
    for _ in 0..n {
        assert_eq!(bytes[i] >> 5, 3, "a text key");
        let len = (bytes[i] & 31) as usize;
        assert!(len < 24);
        let key = std::str::from_utf8(&bytes[i + 1..i + 1 + len]).unwrap();
        let v = fastcbor::field_span(bytes, key).expect("every key has a span");
        assert_eq!(v.start, i + 1 + len);
        out.push(i..v.end);
        i = v.end;
    }
    assert_eq!(i, bytes.len());
    out
}

fn with_v1(r: &Record, bytes: Vec<u8>) -> Record {
    let mut r = r.clone();
    match &mut r {
        Record::LegacyConfiguration(l)
        | Record::LegacyLink(l)
        | Record::LegacyBan(l)
        | Record::LegacyActivity(l)
        | Record::LegacyMessage(l)
        | Record::LegacyMemberInfo(l)
        | Record::LegacyDirectMessage(l)
        | Record::LegacyPurge(l)
        | Record::LegacySecretBlob(l) => l.v1.0 = bytes,
        _ => unreachable!(),
    }
    r
}

/// Whether serde (ciborium, no re-encode check) decodes `b` as an `A`.
fn lenient<A: DeserializeOwned>(b: &[u8]) -> bool {
    ciborium::de::from_reader::<A, _>(b).is_ok()
}

fn with_bytes(r: &Record) -> Vec<u8> {
    match r {
        Record::LegacyConfiguration(l)
        | Record::LegacyLink(l)
        | Record::LegacyBan(l)
        | Record::LegacyActivity(l)
        | Record::LegacyMessage(l)
        | Record::LegacyMemberInfo(l)
        | Record::LegacyDirectMessage(l)
        | Record::LegacyPurge(l)
        | Record::LegacySecretBlob(l) => l.v1.0.clone(),
        _ => unreachable!(),
    }
}

/// The ciborium path's answer: decode with the re-encode check, encode the
/// signed field again, verify.
fn old_verify<A: DeserializeOwned + Serialize, I: Serialize>(
    l: &Legacy,
    inner: impl Fn(&A) -> (&I, &ed25519_dalek::Signature),
) -> bool {
    let Some(a) = from_cbor_canonical::<A>(&l.v1.0) else {
        return false;
    };
    let (i, sig) = inner(&a);
    verify_bytes(&l.signer, &to_cbor(i), &Sig(sig.to_bytes()))
}

/// The signature check of one decoded legacy record, through `decode_*`'s
/// kept span (or built bytes): `None` if it does not decode.
fn kept_verify(o: &Key, r: &Record) -> Option<bool> {
    use crate::room_state_v2::legacy::*;
    let (v1, l) = match r {
        Record::LegacyConfiguration(l) => (decode_config(o, l)?.v1, l),
        Record::LegacyLink(l) => (decode_link(o, l)?.v1, l),
        Record::LegacyBan(l) => (decode_ban(o, l)?.v1, l),
        Record::LegacyActivity(l) | Record::LegacyMessage(l) => (decode_message(o, l)?.v1, l),
        Record::LegacyMemberInfo(l) => (decode_member_info(o, l)?.v1, l),
        Record::LegacyDirectMessage(l) => (decode_dm(o, l)?.v1, l),
        Record::LegacyPurge(l) => (decode_purge(o, l)?.v1, l),
        Record::LegacySecretBlob(l) => (decode_blob(o, l)?.v1, l),
        _ => return None,
    };
    Some(v1.expect("decode keeps the signature").verify(o, l))
}

/// Checks one legacy record's signature paths against the ciborium path:
/// the signed span equals the field's old encoding, `legacy::verify` and
/// the kept check give the old answer, and swapping two map entries (a
/// form serde decodes, with the signed span intact) is refused by every
/// path. Returns whether the signature verifies.
pub fn check_signed(o: &Key, r: &Record) -> bool {
    use crate::room_state_v2::legacy;
    let (l, field, old, lenient) = match r {
        Record::LegacyConfiguration(l) => (
            l,
            Some((
                "configuration",
                to_cbor(
                    &from_cbor_canonical::<AuthorizedConfigurationV1>(&l.v1.0)
                        .unwrap()
                        .configuration,
                ),
            )),
            old_verify::<AuthorizedConfigurationV1, _>(l, |a| (&a.configuration, &a.signature)),
            lenient::<AuthorizedConfigurationV1> as fn(&[u8]) -> bool,
        ),
        Record::LegacyLink(l) => (
            l,
            Some((
                "member",
                to_cbor(
                    &from_cbor_canonical::<AuthorizedMember>(&l.v1.0)
                        .unwrap()
                        .member,
                ),
            )),
            old_verify::<AuthorizedMember, _>(l, |a| (&a.member, &a.signature)),
            lenient::<AuthorizedMember> as fn(&[u8]) -> bool,
        ),
        Record::LegacyBan(l) => (
            l,
            Some((
                "ban",
                to_cbor(
                    &from_cbor_canonical::<AuthorizedUserBan>(&l.v1.0)
                        .unwrap()
                        .ban,
                ),
            )),
            old_verify::<AuthorizedUserBan, _>(l, |a| (&a.ban, &a.signature)),
            lenient::<AuthorizedUserBan> as fn(&[u8]) -> bool,
        ),
        Record::LegacyActivity(l) | Record::LegacyMessage(l) => (
            l,
            Some((
                "message",
                to_cbor(
                    &from_cbor_canonical::<AuthorizedMessageV1>(&l.v1.0)
                        .unwrap()
                        .message,
                ),
            )),
            old_verify::<AuthorizedMessageV1, _>(l, |a| (&a.message, &a.signature)),
            lenient::<AuthorizedMessageV1> as fn(&[u8]) -> bool,
        ),
        Record::LegacyMemberInfo(l) => (
            l,
            Some((
                "member_info",
                to_cbor(
                    &from_cbor_canonical::<AuthorizedMemberInfo>(&l.v1.0)
                        .unwrap()
                        .member_info,
                ),
            )),
            old_verify::<AuthorizedMemberInfo, _>(l, |a| (&a.member_info, &a.signature)),
            lenient::<AuthorizedMemberInfo> as fn(&[u8]) -> bool,
        ),
        Record::LegacySecretBlob(l) => (
            l,
            Some((
                "secret",
                to_cbor(
                    &from_cbor_canonical::<AuthorizedEncryptedSecretForMember>(&l.v1.0)
                        .unwrap()
                        .secret,
                ),
            )),
            old_verify::<AuthorizedEncryptedSecretForMember, _>(l, |a| {
                (&a.secret, &a.owner_signature)
            }),
            lenient::<AuthorizedEncryptedSecretForMember> as fn(&[u8]) -> bool,
        ),
        // Their signed bytes are built from fields, as before: only the
        // refusal of non-canonical forms is checked here.
        Record::LegacyDirectMessage(l) => (
            l,
            None,
            legacy::verify(o, r),
            lenient::<AuthorizedDirectMessage> as fn(&[u8]) -> bool,
        ),
        Record::LegacyPurge(l) => (
            l,
            None,
            legacy::verify(o, r),
            lenient::<AuthorizedRecipientPurges> as fn(&[u8]) -> bool,
        ),
        _ => panic!("not a legacy record"),
    };
    if let Some((field, bytes)) = field {
        let span = fastcbor::field_span(&l.v1.0, field).expect("the signed field");
        assert_eq!(
            &l.v1.0[span],
            &bytes[..],
            "the span is the field's encoding"
        );
    }
    let new = legacy::verify(o, r);
    assert_eq!(new, old, "legacy::verify differs from the ciborium path");
    if let Some(k) = kept_verify(o, r) {
        assert_eq!(k, old, "the kept check differs from the ciborium path");
    }
    let e = entries(&l.v1.0);
    assert!(e.len() >= 2);
    let mut swapped = l.v1.0[..1].to_vec();
    swapped.extend_from_slice(&l.v1.0[e[1].clone()]);
    swapped.extend_from_slice(&l.v1.0[e[0].clone()]);
    for x in &e[2..] {
        swapped.extend_from_slice(&l.v1.0[x.clone()]);
    }
    assert_ne!(swapped, l.v1.0);
    let bad = with_v1(r, swapped);
    assert!(!legacy::verify(o, &bad), "a reordered record verifies");
    assert_eq!(kept_verify(o, &bad), None, "a reordered record decodes");
    assert!(
        lenient(&with_bytes(&bad)),
        "serde decodes the reordered record"
    );
    new
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
    let bo = Key::from(&crate::room_state_v2::bench::owner_key().verifying_key());
    let (s, dropped) = largest_state(Caps::ceilings_legacy().scaled(4), 0);
    assert_eq!(dropped, 0);
    for r in &s.records {
        if let Some(k) = check_legacy(r) {
            seen.insert(k);
            assert!(check_signed(&bo, r), "a builder record's signature fails");
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
            assert!(check_signed(&bo, r), "a builder record's signature fails");
        }
    }
    let v1 = super::convert_tests::v1_room();
    let o = crate::room_state_v2::encoding::Key::from(&super::convert_tests::owner_vk());
    for r in crate::room_state_v2::convert::legacy_records(&o, &v1).records {
        check_legacy(&r);
        check_signed(&o, &r);
    }
    let c = crate::room_state_v2::convert::legacy_configuration(&o, &v1);
    check_legacy(&c);
    check_signed(&o, &c);
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
    let mut verified = 0;
    for r in &recs {
        check_legacy(r);
        verified += check_signed(&o, r) as usize;
    }
    let c = crate::room_state_v2::convert::legacy_configuration(&o, &v1);
    check_legacy(&c);
    verified += check_signed(&o, &c) as usize;
    println!(
        "{} live v1 records encode and decode identically; {verified} verify, as on the ciborium path",
        recs.len() + 1
    );
}
