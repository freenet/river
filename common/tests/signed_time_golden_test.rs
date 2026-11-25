//! The exact signed time (freenet/river#1018).
//!
//! `SignedTime` replaces `SystemTime` in the four signed structs. These tests pin that the swap
//! changes no byte of any encoding, so every signature made before it still verifies, and that a
//! time whose nanoseconds are not a multiple of 100 survives a decode, which a Windows
//! `SystemTime` cannot do.

use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::member::MemberId;
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::RoomCipherSpec;
use river_core::room_state::provisional::{
    provisional_ban_signing_bytes, AuthorizedProvisionalBan, ProvisionalBan,
};
use river_core::room_state::secret::{AuthorizedSecretVersionRecord, SecretVersionRecordV1};
use river_core::SignedTime;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// Signed structs as they were before: the same fields with a `SystemTime`. Their bytes are the
// ones every existing signature covers.
#[derive(Serialize, Deserialize)]
struct OldMessage {
    room_owner: MemberId,
    author: MemberId,
    time: SystemTime,
    content: RoomMessageBody,
}
#[derive(Serialize, Deserialize)]
struct OldBan {
    owner_member_id: MemberId,
    banned_at: SystemTime,
    banned_user: MemberId,
}
#[derive(Serialize, Deserialize)]
struct OldSecret {
    version: u32,
    cipher_spec: RoomCipherSpec,
    created_at: SystemTime,
}
#[derive(Serialize, Deserialize)]
struct OldProvBan {
    banned_by: MemberId,
    banned_at: SystemTime,
    target: MemberId,
}

fn cbor<T: Serialize>(t: &T) -> Vec<u8> {
    let mut v = vec![];
    ciborium::ser::into_writer(t, &mut v).unwrap();
    v
}

fn from_cbor<T: for<'de> Deserialize<'de>>(b: &[u8]) -> T {
    ciborium::de::from_reader(b).unwrap()
}

fn json<T: Serialize>(t: &T) -> String {
    serde_json::to_string(t).unwrap()
}

fn bin<T: Serialize>(t: &T) -> Vec<u8> {
    bincode::serialize(t).unwrap()
}

/// What a Windows clock keeps of a time: whole units of 100 ns.
fn windows_floor(t: SystemTime) -> SystemTime {
    let d = t.duration_since(UNIX_EPOCH).unwrap();
    UNIX_EPOCH + Duration::new(d.as_secs(), d.subsec_nanos() / 100 * 100)
}

fn owner() -> SigningKey {
    SigningKey::from_bytes(&[1u8; 32])
}
fn member() -> SigningKey {
    SigningKey::from_bytes(&[2u8; 32])
}
fn id(k: &SigningKey) -> MemberId {
    MemberId::from(&k.verifying_key())
}
fn sig(h: &str) -> Signature {
    Signature::from_slice(&hex::decode(h).unwrap()).unwrap()
}

/// Whole seconds, none; nanoseconds not a multiple of 100 (a Linux or macOS `now()`), the edges
/// of the nanosecond field, whole milliseconds, and large seconds.
fn sample_times() -> Vec<(u64, u32)> {
    let mut v = vec![
        (0, 0),
        (0, 1),
        (0, 99),
        (0, 100),
        (1, 999_999_999),
        (1_700_000_000, 0),
        (1_700_000_000, 1),
        (1_700_000_000, 123_456_789),
        (1_700_000_000, 123_000_000),
        (1_700_000_000, 999_999_999),
        (1_700_000_000, 999_999_901),
        (4_102_444_800, 500_000_050),
        (u32::MAX as u64, 7),
        (i64::MAX as u64, 999_999_999),
    ];
    for i in 0..200u32 {
        v.push((
            1_600_000_000 + i as u64 * 7919,
            i.wrapping_mul(2_654_435_761) % 1_000_000_000,
        ));
    }
    v
}

// ---- (a) byte identity with SystemTime ----

#[test]
fn encodes_exactly_as_system_time_in_every_format() {
    for (secs, nanos) in sample_times() {
        let Some(st) = UNIX_EPOCH.checked_add(Duration::new(secs, nanos)) else {
            continue; // the platform clock cannot hold it; covered by the range test below
        };
        let t = SignedTime::new(secs, nanos).unwrap();
        assert_eq!(cbor(&t), cbor(&st), "cbor {secs}.{nanos:09}");
        assert_eq!(json(&t), json(&st), "json {secs}.{nanos:09}");
        assert_eq!(bin(&t), bin(&st), "bincode {secs}.{nanos:09}");
        // And inside the signed structs, which is what a signature covers.
        let new = UserBan {
            owner_member_id: id(&owner()),
            banned_at: t,
            banned_user: id(&member()),
        };
        let old = OldBan {
            owner_member_id: id(&owner()),
            banned_at: st,
            banned_user: id(&member()),
        };
        assert_eq!(cbor(&new), cbor(&old));
        assert_eq!(bin(&new), bin(&old));
        let new = SecretVersionRecordV1 {
            version: 3,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: t,
        };
        let old = OldSecret {
            version: 3,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: st,
        };
        assert_eq!(cbor(&new), cbor(&old));
        assert_eq!(bin(&new), bin(&old));
        let new = ProvisionalBan {
            banned_by: id(&owner()),
            banned_at: t,
            target: id(&member()),
        };
        let old = OldProvBan {
            banned_by: id(&owner()),
            banned_at: st,
            target: id(&member()),
        };
        assert_eq!(cbor(&new), cbor(&old));
        assert_eq!(bin(&new), bin(&old));
        let new = MessageV1 {
            room_owner: id(&owner()),
            author: id(&member()),
            time: t,
            content: RoomMessageBody::public("x".into()),
        };
        let old = OldMessage {
            room_owner: id(&owner()),
            author: id(&member()),
            time: st,
            content: RoomMessageBody::public("x".into()),
        };
        assert_eq!(cbor(&new), cbor(&old));
        assert_eq!(bin(&new), bin(&old));
    }
}

#[test]
fn decodes_everything_system_time_decodes_to_the_same_value() {
    for (secs, nanos) in sample_times() {
        let Some(st) = UNIX_EPOCH.checked_add(Duration::new(secs, nanos)) else {
            continue;
        };
        let bytes = cbor(&st);
        let t: SignedTime = from_cbor(&bytes);
        assert_eq!((t.secs(), t.nanos()), (secs, nanos));
        let j: SignedTime = serde_json::from_str(&json(&st)).unwrap();
        assert_eq!(j, t);
        let b: SignedTime = bincode::deserialize(&bin(&st)).unwrap();
        assert_eq!(b, t);
    }
}

/// Every input form `SystemTime` accepts or rejects, the new type accepts or rejects the same.
#[test]
fn accepts_and_rejects_what_system_time_does() {
    let inputs = [
        // map form, either field order
        r#"{"secs_since_epoch":5,"nanos_since_epoch":6}"#,
        r#"{"nanos_since_epoch":6,"secs_since_epoch":5}"#,
        // sequence form
        "[5,6]",
        // nanoseconds of a second or more carry into the seconds
        r#"{"secs_since_epoch":5,"nanos_since_epoch":1500000000}"#,
        r#"{"secs_since_epoch":5,"nanos_since_epoch":4294967295}"#,
        // seconds that fit an i64, and the first that does not
        r#"{"secs_since_epoch":9223372036854775807,"nanos_since_epoch":0}"#,
        r#"{"secs_since_epoch":9223372036854775808,"nanos_since_epoch":0}"#,
        // the carry overflows u64
        r#"{"secs_since_epoch":18446744073709551615,"nanos_since_epoch":1000000000}"#,
        r#"{"secs_since_epoch":18446744073709551615,"nanos_since_epoch":0}"#,
        // malformed
        r#"{"secs_since_epoch":5}"#,
        r#"{"nanos_since_epoch":6}"#,
        r#"{"secs_since_epoch":5,"secs_since_epoch":5,"nanos_since_epoch":6}"#,
        r#"{"secs_since_epoch":5,"nanos_since_epoch":6,"extra":1}"#,
        "[5]",
        "[5,6,7]",
        r#"{"secs_since_epoch":-1,"nanos_since_epoch":0}"#,
        r#"{"secs_since_epoch":5,"nanos_since_epoch":-1}"#,
        "5",
    ];
    for input in inputs {
        let old = serde_json::from_str::<SystemTime>(input);
        let new = serde_json::from_str::<SignedTime>(input);
        match (old, new) {
            (Ok(o), Ok(n)) => assert_eq!(n.to_system_time(), o, "{input}"),
            (Err(_), Err(_)) => {}
            (o, n) => panic!("{input}: SystemTime {o:?} but SignedTime {n:?}"),
        }
    }
    // The same through CBOR, whose struct is a map, for the field-order and carry cases.
    #[derive(Serialize)]
    struct Raw {
        secs_since_epoch: u64,
        nanos_since_epoch: u32,
    }
    for (secs, nanos) in [
        (5u64, 1_500_000_000u32),
        (u64::MAX, 1_000_000_000),
        (1 << 63, 0),
        (5, 6),
    ] {
        let b = cbor(&Raw {
            secs_since_epoch: secs,
            nanos_since_epoch: nanos,
        });
        let old = ciborium::de::from_reader::<SystemTime, _>(b.as_slice());
        let new = ciborium::de::from_reader::<SignedTime, _>(b.as_slice());
        match (old, new) {
            (Ok(o), Ok(n)) => assert_eq!(n.to_system_time(), o),
            (Err(_), Err(_)) => {}
            (o, n) => panic!("cbor {secs}.{nanos}: SystemTime {o:?} but SignedTime {n:?}"),
        }
    }
}

#[test]
fn order_matches_system_time() {
    let ts: Vec<(u64, u32)> = sample_times();
    for a in &ts {
        for b in &ts {
            let (Some(sa), Some(sb)) = (
                UNIX_EPOCH.checked_add(Duration::new(a.0, a.1)),
                UNIX_EPOCH.checked_add(Duration::new(b.0, b.1)),
            ) else {
                continue;
            };
            let (ta, tb) = (
                SignedTime::new(a.0, a.1).unwrap(),
                SignedTime::new(b.0, b.1).unwrap(),
            );
            assert_eq!(ta.cmp(&tb), sa.cmp(&sb));
        }
    }
}

#[test]
fn a_time_before_the_epoch_cannot_be_signed_either_way() {
    let before = UNIX_EPOCH - Duration::from_secs(1);
    assert!(serde_json::to_string(&before).is_err());
    assert_eq!(SignedTime::from(before), SignedTime::UNIX_EPOCH);
}

// ---- (b) signatures made before this type existed ----
//
// Generated with the pre-change layout (a `SystemTime` field) at 1_700_000_000 s +
// 123_456_789 ns, which is not a multiple of 100 ns, and signed with fixed keys. Keys: owner
// `[1; 32]`, member `[2; 32]`.

const MSG: &str = "a46a726f6f6d5f6f776e65721b20340a250a45d46066617574686f723b6d9aeebb4641956f6474696d65a270736563735f73696e63655f65706f63681a6553f100716e616e6f735f73696e63655f65706f63681a075bcd1567636f6e74656e74a1665075626c6963a36c636f6e74656e745f74797065016f636f6e74656e745f76657273696f6e0164646174618c18a118641874186518781874186518681865186c186c186f";
const MSG_SIG: &str = "fdec5f6ca5634eb8a7257b075df81440f2e5b6ecca1b0e612c24feeafd9ad45e25daab1bf36fa2db3a2f6d3f824f7ede89eb6f857a43b4a6f4eaff4f2aaf670a";
const BAN: &str = "a36f6f776e65725f6d656d6265725f69641b20340a250a45d4606962616e6e65645f6174a270736563735f73696e63655f65706f63681a6553f100716e616e6f735f73696e63655f65706f63681a075bcd156b62616e6e65645f757365723b6d9aeebb4641956f";
const BAN_SIG: &str = "1a57bb673c5db35bf556f8b8ebaba8476ce02d594e1a3c400e78a25e824c4f29c32d82cc9fef306609e6b5ca0cb0321a34d7ac33b26241d252d997574e512e09";
const SEC: &str = "a36776657273696f6e036b6369706865725f737065636941657332353647636d6a637265617465645f6174a270736563735f73696e63655f65706f63681a6553f100716e616e6f735f73696e63655f65706f63681a075bcd15";
const SEC_SIG: &str = "2878eec8c64944f1e06d6351d3a4e08d597f633d76475069c8a2e18a1a6cba076e4fed9273af2c0f7a61806c1ace3c774d025841687efbfeac6ece3d2f47010b";
const PBAN: &str = "a36962616e6e65645f62791b20340a250a45d4606962616e6e65645f6174a270736563735f73696e63655f65706f63681a6553f100716e616e6f735f73696e63655f65706f63681a075bcd15667461726765743b6d9aeebb4641956f";
const PBAN_SIG: &str = "b667f29d97d8a4d87ad4a33e99c7e0b5b9f4c5360a37f561707b1de0df2c7502dd0592e515def9adc8048451adf1b6820e76a82169baa24b3a063ca2fe1a210a";

fn vk(k: &SigningKey) -> VerifyingKey {
    k.verifying_key()
}

#[test]
fn a_signed_message_decodes_verifies_and_re_encodes_to_the_signed_bytes() {
    let bytes = hex::decode(MSG).unwrap();
    let m: MessageV1 = from_cbor(&bytes);
    assert_eq!(
        (m.time.secs(), m.time.nanos()),
        (1_700_000_000, 123_456_789)
    );
    assert_eq!(cbor(&m), bytes);
    AuthorizedMessageV1::with_signature(m, sig(MSG_SIG))
        .validate(&vk(&member()))
        .expect("signature made before SignedTime must verify");
}

#[test]
fn a_signed_ban_decodes_verifies_and_re_encodes_to_the_signed_bytes() {
    let bytes = hex::decode(BAN).unwrap();
    let b: UserBan = from_cbor(&bytes);
    assert_eq!(
        (b.banned_at.secs(), b.banned_at.nanos()),
        (1_700_000_000, 123_456_789)
    );
    assert_eq!(cbor(&b), bytes);
    AuthorizedUserBan::with_signature(b, id(&owner()), sig(BAN_SIG))
        .verify_signature(&vk(&owner()))
        .expect("signature made before SignedTime must verify");
}

#[test]
fn a_signed_secret_version_record_decodes_verifies_and_re_encodes_to_the_signed_bytes() {
    let bytes = hex::decode(SEC).unwrap();
    let r: SecretVersionRecordV1 = from_cbor(&bytes);
    assert_eq!(
        (r.created_at.secs(), r.created_at.nanos()),
        (1_700_000_000, 123_456_789)
    );
    assert_eq!(cbor(&r), bytes);
    AuthorizedSecretVersionRecord::with_signature(r, sig(SEC_SIG))
        .verify_signature(&vk(&owner()))
        .expect("signature made before SignedTime must verify");
}

#[test]
fn a_signed_provisional_ban_decodes_verifies_and_re_encodes_to_the_signed_bytes() {
    let bytes = hex::decode(PBAN).unwrap();
    let b: ProvisionalBan = from_cbor(&bytes);
    assert_eq!(
        (b.banned_at.secs(), b.banned_at.nanos()),
        (1_700_000_000, 123_456_789)
    );
    assert_eq!(cbor(&b), bytes);
    let signed = AuthorizedProvisionalBan {
        ban: b,
        signature: sig(PBAN_SIG),
    };
    // The issuer here is the owner.
    signed
        .verify_signature(&vk(&owner()), &vk(&owner()))
        .expect("signature made before SignedTime must verify");
    assert!(provisional_ban_signing_bytes(&vk(&owner()), &signed.ban).ends_with(&bytes));
}

// ---- (c) the Windows failure ----

#[test]
fn a_time_with_nanoseconds_off_the_100ns_grid_survives_a_decode() {
    let bytes = hex::decode(MSG).unwrap();

    // The old path on Windows: decode to a `SystemTime`, which keeps whole 100 ns units.
    let old: OldMessage = from_cbor(&bytes);
    let floored = OldMessage {
        time: windows_floor(old.time),
        ..old
    };
    assert_ne!(
        cbor(&floored),
        bytes,
        "the old path loses the signed nanoseconds"
    );
    let msg_vk = vk(&member());
    assert!(
        msg_vk
            .verify_strict(&cbor(&floored), &sig(MSG_SIG))
            .is_err(),
        "so the old path fails the signature"
    );

    // The new path keeps them, whatever the platform clock does.
    let new: MessageV1 = from_cbor(&bytes);
    assert_eq!(new.time.nanos(), 123_456_789);
    assert_ne!(new.time.nanos() % 100, 0);
    assert_eq!(cbor(&new), bytes);
    // Converting to the platform time at the edge is the lossy step, and it is never signed.
    assert!(windows_floor(new.time.to_system_time()) <= new.time.to_system_time());
}

#[test]
fn every_sample_time_round_trips_exactly() {
    for (secs, nanos) in sample_times() {
        let t = SignedTime::new(secs, nanos).unwrap();
        let back: SignedTime = from_cbor(&cbor(&t));
        assert_eq!(back, t);
        assert_eq!((back.secs(), back.nanos()), (secs, nanos));
    }
}

// ---- #1013: new signatures are whole milliseconds ----

#[test]
fn now_is_a_whole_millisecond() {
    for _ in 0..100 {
        let t = SignedTime::now();
        assert_eq!(t.nanos() % 1_000_000, 0);
        assert_eq!(t.whole_millis(), t);
    }
}
