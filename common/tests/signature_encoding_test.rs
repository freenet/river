//! Wire-format tests for the CBOR byte-string encoding of
//! `ed25519_dalek::Signature` (freenet/river#575).
//!
//! Three things have to hold, and each is tested separately because they fail
//! independently:
//!
//! 1. **New form is what we emit.** Every changed field serializes as a CBOR
//!    byte string (`0x58 0x40` + 64 bytes = 66 B), not the derived array of 64
//!    integers.
//! 2. **Old form still decodes.** Rooms and `?invitation=…` links minted before
//!    this change hold the array form; migration re-PUTs that state. The
//!    legacy fixtures here are built through a **shadow struct that uses the
//!    plain `Signature` derive**, so they are genuinely old-encoding rather
//!    than accidentally new-encoding (which would make every test in this file
//!    pass vacuously).
//! 3. **Decoding is canonicalizing.** Old bytes in, new bytes out. No path
//!    carries old-form bytes through to storage or the wire unchanged.
//!
//! The golden vectors below were produced with Python's `cryptography`
//! (Ed25519 signing is deterministic per RFC 8032) and the CBOR headers were
//! hand-derived from RFC 8949, NOT captured from this crate.

use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{
    AuthorizedDirectMessage, AuthorizedRecipientPurges, DirectMessage, PurgeToken, RecipientPurges,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::{RoomCipherSpec, SecretVersion};
use river_core::room_state::secret::{
    AuthorizedEncryptedSecretForMember, AuthorizedSecretVersionRecord, EncryptedSecretForMemberV1,
    SecretVersionRecordV1,
};
use river_core::room_state::upgrade::{AuthorizedUpgradeV1, UpgradeV1};
use serde::Serialize;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Golden vectors (external oracle)
// ---------------------------------------------------------------------------

/// `SigningKey::from_bytes(&[7u8; 32]).sign(b"river#575 golden vector")`,
/// computed with Python `cryptography`'s Ed25519 implementation.
const GOLDEN_SIG_HEX: &str = "e3847cd1ff790c1e4b59ba83506f2b4cc27678e6ce2f8647494476d177dde2cd\
f266306dff334ffb5f26e47b6b524fdd5513da336dcb20da39fb262d1ae3bb04";

/// The same signature as a CBOR byte string: `0x58` (major type 2, 1-byte
/// length follows) `0x40` (64) then the 64 payload bytes. 66 bytes total.
const GOLDEN_BYTESTRING_HEX: &str = "5840e3847cd1ff790c1e4b59ba83506f2b4cc27678e6ce2f8647494476\
d177dde2cdf266306dff334ffb5f26e47b6b524fdd5513da336dcb20da39fb262d1ae3bb04";

/// The same signature in the legacy form: `0x98` (major type 4, 1-byte count
/// follows) `0x40` (64 elements), then each byte as a CBOR unsigned integer —
/// 1 byte for values <= 0x17, 2 bytes (`0x18` + value) above that. This
/// signature has 61 bytes >= 0x18, so it occupies 2 + 64 + 61 = 127 bytes.
const GOLDEN_ARRAY_HEX: &str = "984018e31884187c18d118ff18790c181e184b185918ba18831850186f182b18\
4c18c21876187818e618ce182f1886184718491844187618d1187718dd18e218cd18f2186618301\
86d18ff1833184f18fb185f182618e4187b186b1852184f18dd18551318da1833186d18cb182018\
da183918fb1826182d181a18e318bb04";

fn golden_signature() -> Signature {
    let bytes: [u8; 64] = hex::decode(GOLDEN_SIG_HEX).unwrap().try_into().unwrap();
    Signature::from_bytes(&bytes)
}

/// A wrapper carrying exactly one signature field with the production
/// (byte-string) encoding.
#[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
struct NewWrapper {
    #[serde(with = "river_core::util::sig_serde")]
    sig: Signature,
}

/// The same shape with the *derived* `Signature` encoding — i.e. what River
/// wrote before freenet/river#575. Used to mint genuinely-legacy fixtures.
#[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
struct LegacyWrapper {
    sig: Signature,
}

fn cbor(value: &impl Serialize) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(value, &mut out).unwrap();
    out
}

/// The CBOR encoding of the single-field map `{"sig": …}` is a 1-byte map
/// header (`0xA1`) plus the key (`0x63` + `sig`), so 5 bytes of framing.
const WRAPPER_FRAMING: usize = 5;

#[test]
fn golden_new_form_is_the_hand_derived_byte_string() {
    let encoded = cbor(&NewWrapper {
        sig: golden_signature(),
    });
    assert_eq!(
        hex::encode(&encoded[WRAPPER_FRAMING..]),
        GOLDEN_BYTESTRING_HEX,
        "the byte-string encoding must match the hand-derived RFC 8949 vector"
    );
    assert_eq!(encoded.len() - WRAPPER_FRAMING, 66);
}

#[test]
fn golden_legacy_form_is_the_hand_derived_int_array() {
    let encoded = cbor(&LegacyWrapper {
        sig: golden_signature(),
    });
    assert_eq!(
        hex::encode(&encoded[WRAPPER_FRAMING..]),
        GOLDEN_ARRAY_HEX,
        "the shadow struct must still emit the legacy array form — if this \
         fails, every 'legacy fixture' in this file is silently in the NEW \
         form and all the accept-both tests are vacuous"
    );
    assert_eq!(encoded.len() - WRAPPER_FRAMING, 127);
}

#[test]
fn golden_both_forms_decode_to_the_same_signature() {
    let expected = golden_signature();

    let from_new: NewWrapper = ciborium::de::from_reader(
        hex::decode(GOLDEN_BYTESTRING_HEX_WRAPPED())
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    assert_eq!(from_new.sig, expected);

    let from_legacy: NewWrapper =
        ciborium::de::from_reader(hex::decode(GOLDEN_ARRAY_HEX_WRAPPED()).unwrap().as_slice())
            .unwrap();
    assert_eq!(
        from_legacy.sig, expected,
        "the legacy array form must still decode — this is the arm that keeps \
         pre-#575 rooms and invitation links working"
    );
}

/// `{"sig": <golden byte string>}` as hex.
#[allow(non_snake_case)]
fn GOLDEN_BYTESTRING_HEX_WRAPPED() -> String {
    format!("a163736967{}", GOLDEN_BYTESTRING_HEX)
}

/// `{"sig": <golden int array>}` as hex.
#[allow(non_snake_case)]
fn GOLDEN_ARRAY_HEX_WRAPPED() -> String {
    format!("a163736967{}", GOLDEN_ARRAY_HEX)
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

#[test]
fn short_byte_string_is_rejected() {
    // `0x57` = major type 2, 23-byte inline length.
    let bytes = hex::decode(format!("a163736967{}", "5700000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000")).unwrap();
    let err = ciborium::de::from_reader::<NewWrapper, _>(bytes.as_slice())
        .expect_err("a 23-byte signature must be rejected");
    assert!(format!("{err}").contains("not 64 bytes"), "got: {err}");
}

#[test]
fn short_legacy_array_is_rejected() {
    // 63-element array of zeros.
    let mut hexs = String::from("a1637369679f");
    for _ in 0..63 {
        hexs.push_str("00");
    }
    hexs.push_str("ff"); // indefinite-length array break
    let bytes = hex::decode(&hexs).unwrap();
    let err = ciborium::de::from_reader::<NewWrapper, _>(bytes.as_slice())
        .expect_err("a 63-element signature array must be rejected");
    assert!(
        format!("{err}").contains("shorter than 64 bytes"),
        "got: {err}"
    );
}

#[test]
fn long_legacy_array_is_rejected() {
    let mut hexs = String::from("a1637369679f");
    for _ in 0..65 {
        hexs.push_str("00");
    }
    hexs.push_str("ff");
    let bytes = hex::decode(&hexs).unwrap();
    let err = ciborium::de::from_reader::<NewWrapper, _>(bytes.as_slice())
        .expect_err("a 65-element signature array must be rejected");
    assert!(
        format!("{err}").contains("longer than 64 bytes"),
        "got: {err}"
    );
}

#[test]
fn legacy_array_element_above_255_is_rejected() {
    // 64 elements where the first is 256 (`0x19 0x0100`).
    let mut hexs = String::from("a1637369679f19 0100".replace(' ', "").as_str());
    for _ in 0..63 {
        hexs.push_str("00");
    }
    hexs.push_str("ff");
    let bytes = hex::decode(&hexs).unwrap();
    let err = ciborium::de::from_reader::<NewWrapper, _>(bytes.as_slice())
        .expect_err("an element above 255 must be rejected");
    assert!(format!("{err}").contains("not a byte value"), "got: {err}");
}

#[test]
fn lying_array_length_header_errors_rather_than_panicking() {
    // `0x9B FF..FF` declares 2^64-1 elements and then supplies none. The
    // fixed 64-byte destination means there is nothing to over-allocate, but
    // pin the behaviour anyway: this decode runs inside the room contract on
    // untrusted peer data.
    let bytes = hex::decode("a1637369679bffffffffffffffff").unwrap();
    let err = ciborium::de::from_reader::<NewWrapper, _>(bytes.as_slice())
        .expect_err("a lying length header must produce an error, not a panic");
    let _ = err;
}

#[test]
fn helper_is_not_usable_with_a_non_self_describing_format() {
    // Pins the "do NOT reuse this helper for bincode" warning in the doc
    // comment. `deserialize_any` cannot work without a self-describing
    // format, so this must fail rather than silently mis-decode.
    let encoded = bincode::serialize(&NewWrapper {
        sig: golden_signature(),
    })
    .expect("bincode serialization only needs serialize_bytes");
    assert!(
        bincode::deserialize::<NewWrapper>(&encoded).is_err(),
        "bincode round-trip must fail loudly; if this ever passes, the \
         doc-comment restriction needs revisiting rather than deleting"
    );
}

// ---------------------------------------------------------------------------
// Fixtures for the ten changed fields
// ---------------------------------------------------------------------------

fn sk(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn vk(seed: u8) -> VerifyingKey {
    sk(seed).verifying_key()
}

fn member(owner: u8, inviter: u8, invitee: u8) -> Member {
    Member {
        owner_member_id: MemberId::from(&vk(owner)),
        invited_by: MemberId::from(&vk(inviter)),
        member_vk: vk(invitee),
    }
}

/// Every changed type, built with real keys and real signatures, alongside a
/// shadow struct that re-serializes the same record with the legacy encoding.
///
/// Each entry is `(name, new_bytes, legacy_bytes, signature_count)`.
struct Case {
    name: &'static str,
    new_bytes: Vec<u8>,
    legacy_bytes: Vec<u8>,
    signatures: Vec<Signature>,
}

fn all_cases() -> Vec<Case> {
    let owner = sk(1);
    let inviter = sk(2);
    let invitee = sk(3);
    let mut cases = Vec::new();

    // 1. AuthorizedMember (member.rs)
    {
        #[derive(Serialize)]
        struct Legacy<'a> {
            member: &'a Member,
            signature: &'a Signature,
        }
        let v = AuthorizedMember::new(member(1, 2, 3), &inviter);
        cases.push(Case {
            name: "AuthorizedMember",
            new_bytes: cbor(&v),
            legacy_bytes: cbor(&Legacy {
                member: &v.member,
                signature: &v.signature,
            }),
            signatures: vec![v.signature],
        });
    }

    // 2. AuthorizedMemberInfo (member_info.rs)
    {
        #[derive(Serialize)]
        struct Legacy<'a> {
            member_info: &'a MemberInfo,
            signature: &'a Signature,
        }
        let info = MemberInfo::new_public(MemberId::from(&vk(3)), 1, "Nickname".to_string());
        let v = AuthorizedMemberInfo::new(info, &owner);
        cases.push(Case {
            name: "AuthorizedMemberInfo",
            new_bytes: cbor(&v),
            legacy_bytes: cbor(&Legacy {
                member_info: &v.member_info,
                signature: &v.signature,
            }),
            signatures: vec![v.signature],
        });
    }

    // 3. AuthorizedMessageV1 (message.rs)
    {
        #[derive(Serialize)]
        struct Legacy<'a> {
            message: &'a MessageV1,
            signature: &'a Signature,
        }
        let msg = MessageV1 {
            room_owner: MemberId::from(&vk(1)),
            author: MemberId::from(&vk(3)),
            time: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            content: RoomMessageBody::public("hello river".to_string()),
        };
        let v = AuthorizedMessageV1::new(msg, &invitee);
        cases.push(Case {
            name: "AuthorizedMessageV1",
            new_bytes: cbor(&v),
            legacy_bytes: cbor(&Legacy {
                message: &v.message,
                signature: &v.signature,
            }),
            signatures: vec![v.signature],
        });
    }

    // 4. AuthorizedConfigurationV1 (configuration.rs)
    {
        #[derive(Serialize)]
        struct Legacy<'a> {
            configuration: &'a Configuration,
            signature: &'a Signature,
        }
        let config = Configuration {
            owner_member_id: MemberId::from(&vk(1)),
            ..Default::default()
        };
        let v = AuthorizedConfigurationV1::new(config, &owner);
        cases.push(Case {
            name: "AuthorizedConfigurationV1",
            new_bytes: cbor(&v),
            legacy_bytes: cbor(&Legacy {
                configuration: &v.configuration,
                signature: &v.signature,
            }),
            signatures: vec![v.signature],
        });
    }

    // 5. AuthorizedUserBan (ban.rs)
    {
        #[derive(Serialize)]
        struct Legacy<'a> {
            ban: &'a UserBan,
            banned_by: &'a MemberId,
            signature: &'a Signature,
        }
        let ban = UserBan {
            owner_member_id: MemberId::from(&vk(1)),
            banned_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_001),
            banned_user: MemberId::from(&vk(3)),
        };
        let v = AuthorizedUserBan::new(ban, MemberId::from(&vk(1)), &owner);
        cases.push(Case {
            name: "AuthorizedUserBan",
            new_bytes: cbor(&v),
            legacy_bytes: cbor(&Legacy {
                ban: &v.ban,
                banned_by: &v.banned_by,
                signature: &v.signature,
            }),
            signatures: vec![v.signature],
        });
    }

    // 6. AuthorizedUpgradeV1 (upgrade.rs)
    {
        #[derive(Serialize)]
        struct Legacy<'a> {
            upgrade: &'a UpgradeV1,
            signature: &'a Signature,
        }
        let upgrade = UpgradeV1 {
            owner_member_id: MemberId::from(&vk(1)),
            version: 2,
            new_chatroom_address: blake3::hash(b"new room"),
        };
        let v = AuthorizedUpgradeV1::new(upgrade, &owner);
        cases.push(Case {
            name: "AuthorizedUpgradeV1",
            new_bytes: cbor(&v),
            legacy_bytes: cbor(&Legacy {
                upgrade: &v.upgrade,
                signature: &v.signature,
            }),
            signatures: vec![v.signature],
        });
    }

    // 7. AuthorizedSecretVersionRecord (secret.rs)
    {
        #[derive(Serialize)]
        struct Legacy<'a> {
            record: &'a SecretVersionRecordV1,
            owner_signature: &'a Signature,
        }
        let record = SecretVersionRecordV1 {
            version: 0 as SecretVersion,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_002),
        };
        let v = AuthorizedSecretVersionRecord::new(record, &owner);
        cases.push(Case {
            name: "AuthorizedSecretVersionRecord",
            new_bytes: cbor(&v),
            legacy_bytes: cbor(&Legacy {
                record: &v.record,
                owner_signature: &v.owner_signature,
            }),
            signatures: vec![v.owner_signature],
        });
    }

    // 8. AuthorizedEncryptedSecretForMember (secret.rs)
    {
        #[derive(Serialize)]
        struct Legacy<'a> {
            secret: &'a EncryptedSecretForMemberV1,
            owner_signature: &'a Signature,
        }
        let secret = EncryptedSecretForMemberV1 {
            member_id: MemberId::from(&vk(3)),
            secret_version: 0 as SecretVersion,
            ciphertext: vec![0xAB; 48],
            nonce: [7u8; 12],
            sender_ephemeral_public_key: [9u8; 32],
            provider: MemberId::from(&vk(1)),
        };
        let v = AuthorizedEncryptedSecretForMember::new(secret, &owner);
        cases.push(Case {
            name: "AuthorizedEncryptedSecretForMember",
            new_bytes: cbor(&v),
            legacy_bytes: cbor(&Legacy {
                secret: &v.secret,
                owner_signature: &v.owner_signature,
            }),
            signatures: vec![v.owner_signature],
        });
    }

    // 9. AuthorizedDirectMessage (direct_messages.rs)
    {
        #[derive(Serialize)]
        struct Legacy<'a> {
            message: &'a DirectMessage,
            sender_signature: &'a Signature,
        }
        let v = river_core::room_state::direct_messages::sign_direct_message(
            &invitee,
            MemberId::from(&vk(3)),
            MemberId::from(&vk(2)),
            &vk(1),
            1_700_000_003,
            b"dm ciphertext".to_vec(),
        )
        .unwrap();
        cases.push(Case {
            name: "AuthorizedDirectMessage",
            new_bytes: cbor(&v),
            legacy_bytes: cbor(&Legacy {
                message: &v.message,
                sender_signature: &v.sender_signature,
            }),
            signatures: vec![v.sender_signature],
        });
    }

    // 10. AuthorizedRecipientPurges (direct_messages.rs)
    {
        #[derive(Serialize)]
        struct Legacy<'a> {
            recipient_id: &'a MemberId,
            state: &'a RecipientPurges,
            recipient_signature: &'a Signature,
        }
        let v = river_core::room_state::direct_messages::sign_recipient_purges(
            &inviter,
            MemberId::from(&vk(2)),
            &vk(1),
            RecipientPurges {
                version: 3,
                purged: vec![PurgeToken([0xAA; 16])],
            },
        )
        .unwrap();
        cases.push(Case {
            name: "AuthorizedRecipientPurges",
            new_bytes: cbor(&v),
            legacy_bytes: cbor(&Legacy {
                recipient_id: &v.recipient_id,
                state: &v.state,
                recipient_signature: &v.recipient_signature,
            }),
            signatures: vec![v.recipient_signature],
        });
    }

    cases
}

/// Guard against the whole file going vacuous: if a `Case`'s "legacy" bytes are
/// byte-equal to its new bytes, the shadow struct drifted out of step with the
/// real one (a renamed or reordered field) and every legacy assertion below is
/// testing the new encoding against itself.
#[test]
fn every_case_has_a_genuinely_distinct_legacy_encoding() {
    let cases = all_cases();
    assert_eq!(cases.len(), 10, "all ten changed fields must be covered");
    for case in &cases {
        assert_ne!(
            case.new_bytes, case.legacy_bytes,
            "{}: legacy fixture is byte-identical to the new form — the shadow \
             struct is not producing the derived Signature encoding",
            case.name
        );
        let expected_delta: usize = case
            .signatures
            .iter()
            .map(|s| s.to_bytes().iter().filter(|b| **b >= 0x18).count())
            .sum();
        assert_eq!(
            case.legacy_bytes.len() - case.new_bytes.len(),
            expected_delta,
            "{}: saving must be exactly the count of signature bytes >= 0x18",
            case.name
        );
    }
}

macro_rules! decode {
    ($ty:ty, $bytes:expr) => {
        ciborium::de::from_reader::<$ty, _>($bytes.as_slice())
    };
}

/// New-form round-trip plus legacy acceptance plus canonicalization, for every
/// changed type. Written out per type rather than through a trait object so a
/// failure names the type directly.
#[test]
fn every_changed_type_round_trips_and_accepts_the_legacy_form() {
    let cases = all_cases();
    let by_name = |n: &str| cases.iter().find(|c| c.name == n).unwrap();

    macro_rules! check {
        ($name:literal, $ty:ty) => {{
            let case = by_name($name);
            let from_new = decode!($ty, case.new_bytes)
                .unwrap_or_else(|e| panic!("{}: new form must decode: {e}", $name));
            let from_legacy = decode!($ty, case.legacy_bytes)
                .unwrap_or_else(|e| panic!("{}: legacy form must decode: {e}", $name));
            assert_eq!(
                from_new, from_legacy,
                "{}: both encodings must decode to the same value",
                $name
            );
            // Canonicalization: re-encoding what came out of the legacy bytes
            // must produce the NEW form, so old bytes cannot survive a
            // decode/re-encode cycle and reach storage or the wire.
            assert_eq!(
                cbor(&from_legacy),
                case.new_bytes,
                "{}: re-encoding a legacy-decoded value must yield the new form",
                $name
            );
        }};
    }

    check!("AuthorizedMember", AuthorizedMember);
    check!("AuthorizedMemberInfo", AuthorizedMemberInfo);
    check!("AuthorizedMessageV1", AuthorizedMessageV1);
    check!("AuthorizedConfigurationV1", AuthorizedConfigurationV1);
    check!("AuthorizedUserBan", AuthorizedUserBan);
    check!("AuthorizedUpgradeV1", AuthorizedUpgradeV1);
    check!(
        "AuthorizedSecretVersionRecord",
        AuthorizedSecretVersionRecord
    );
    check!(
        "AuthorizedEncryptedSecretForMember",
        AuthorizedEncryptedSecretForMember
    );
    check!("AuthorizedDirectMessage", AuthorizedDirectMessage);
    check!("AuthorizedRecipientPurges", AuthorizedRecipientPurges);
}

/// The point of the accept-both arm: a record decoded from pre-#575 bytes must
/// still verify. If the legacy arm silently produced a different signature the
/// value would decode and then fail verification at migration time.
#[test]
fn legacy_decoded_records_still_verify() {
    let cases = all_cases();
    let by_name = |n: &str| cases.iter().find(|c| c.name == n).unwrap();

    let m: AuthorizedMember = decode!(AuthorizedMember, by_name("AuthorizedMember").legacy_bytes)
        .expect("legacy AuthorizedMember decodes");
    m.verify_signature(&vk(2))
        .expect("legacy-decoded member must still verify");

    let mi: AuthorizedMemberInfo = decode!(
        AuthorizedMemberInfo,
        by_name("AuthorizedMemberInfo").legacy_bytes
    )
    .expect("legacy AuthorizedMemberInfo decodes");
    mi.verify_signature_with_key(&vk(1))
        .expect("legacy-decoded member info must still verify");

    let msg: AuthorizedMessageV1 = decode!(
        AuthorizedMessageV1,
        by_name("AuthorizedMessageV1").legacy_bytes
    )
    .expect("legacy AuthorizedMessageV1 decodes");
    msg.validate(&vk(3))
        .expect("legacy-decoded message must still verify");

    let ban: AuthorizedUserBan =
        decode!(AuthorizedUserBan, by_name("AuthorizedUserBan").legacy_bytes)
            .expect("legacy AuthorizedUserBan decodes");
    ban.verify_signature(&vk(1))
        .expect("legacy-decoded ban must still verify");

    let cfg: AuthorizedConfigurationV1 = decode!(
        AuthorizedConfigurationV1,
        by_name("AuthorizedConfigurationV1").legacy_bytes
    )
    .expect("legacy AuthorizedConfigurationV1 decodes");
    cfg.verify_signature(&vk(1))
        .expect("legacy-decoded configuration must still verify");

    let up: AuthorizedUpgradeV1 = decode!(
        AuthorizedUpgradeV1,
        by_name("AuthorizedUpgradeV1").legacy_bytes
    )
    .expect("legacy AuthorizedUpgradeV1 decodes");
    up.validate(&vk(1))
        .expect("legacy-decoded upgrade must still verify");

    let rec: AuthorizedSecretVersionRecord = decode!(
        AuthorizedSecretVersionRecord,
        by_name("AuthorizedSecretVersionRecord").legacy_bytes
    )
    .expect("legacy AuthorizedSecretVersionRecord decodes");
    rec.verify_signature(&vk(1))
        .expect("legacy-decoded secret record must still verify");

    let enc: AuthorizedEncryptedSecretForMember = decode!(
        AuthorizedEncryptedSecretForMember,
        by_name("AuthorizedEncryptedSecretForMember").legacy_bytes
    )
    .expect("legacy AuthorizedEncryptedSecretForMember decodes");
    enc.verify_signature(&vk(1))
        .expect("legacy-decoded encrypted secret must still verify");
}

// ---------------------------------------------------------------------------
// Size
// ---------------------------------------------------------------------------

/// Measures the saving on a built room state, rebuilding the OLD encoding from
/// the same records in-test so neither figure can rot (the discipline
/// freenet/river#572 adopted after #571 shipped a stale quoted number).
#[test]
fn state_size_saving_matches_the_signature_population() {
    use river_core::room_state::member::MembersV1;
    use river_core::room_state::member_info::MemberInfoV1;

    let owner = sk(1);
    const MEMBERS: usize = 200;

    let mut members = Vec::with_capacity(MEMBERS);
    let mut infos = Vec::with_capacity(MEMBERS);
    let mut signatures = Vec::with_capacity(MEMBERS * 2);

    for i in 0..MEMBERS {
        let member_sk = SigningKey::from_bytes(&blake3::hash(&(i as u64).to_le_bytes()).into());
        let m = Member {
            owner_member_id: MemberId::from(&owner.verifying_key()),
            invited_by: MemberId::from(&owner.verifying_key()),
            member_vk: member_sk.verifying_key(),
        };
        let am = AuthorizedMember::new(m, &owner);
        let info = MemberInfo::new_public(
            MemberId::from(&member_sk.verifying_key()),
            1,
            format!("member-{i}"),
        );
        let ami = AuthorizedMemberInfo::new(info, &owner);
        signatures.push(am.signature);
        signatures.push(ami.signature);
        members.push(am);
        infos.push(ami);
    }

    #[derive(Serialize)]
    struct LegacyMember<'a> {
        member: &'a Member,
        signature: &'a Signature,
    }
    #[derive(Serialize)]
    struct LegacyMemberInfo<'a> {
        member_info: &'a MemberInfo,
        signature: &'a Signature,
    }
    #[derive(Serialize)]
    struct LegacyMembersV1<'a> {
        members: Vec<LegacyMember<'a>>,
    }
    #[derive(Serialize)]
    struct LegacyMemberInfoV1<'a> {
        member_info: Vec<LegacyMemberInfo<'a>>,
    }

    let new_members = cbor(&MembersV1 {
        members: members.clone(),
    });
    let new_infos = cbor(&MemberInfoV1 {
        member_info: infos.clone(),
    });
    let legacy_members = cbor(&LegacyMembersV1 {
        members: members
            .iter()
            .map(|m| LegacyMember {
                member: &m.member,
                signature: &m.signature,
            })
            .collect(),
    });
    let legacy_infos = cbor(&LegacyMemberInfoV1 {
        member_info: infos
            .iter()
            .map(|m| LegacyMemberInfo {
                member_info: &m.member_info,
                signature: &m.signature,
            })
            .collect(),
    });

    let new_total = new_members.len() + new_infos.len();
    let legacy_total = legacy_members.len() + legacy_infos.len();
    let saved = legacy_total - new_total;

    // Per-signature saving is exactly the number of signature bytes >= 0x18:
    // each such byte costs 2 CBOR bytes in the array form and 1 in the byte
    // string. It is therefore signature-dependent, ~58 B on average — NOT the
    // flat 55 B the issue's single sample happened to show.
    let expected: usize = signatures
        .iter()
        .map(|s| s.to_bytes().iter().filter(|b| **b >= 0x18).count())
        .sum();
    assert_eq!(
        saved, expected,
        "saving must equal the summed per-signature delta"
    );

    let per_sig = saved as f64 / signatures.len() as f64;
    assert!(
        (50.0..=66.0).contains(&per_sig),
        "per-signature saving {per_sig:.1} B is outside the plausible 50-66 B \
         band for a 64-byte signature"
    );

    eprintln!(
        "signatures={} legacy={legacy_total} B new={new_total} B saved={saved} B \
         ({:.1}% of legacy, {per_sig:.1} B/signature)",
        signatures.len(),
        100.0 * saved as f64 / legacy_total as f64
    );

    // Both figures are pinned so neither can rot silently. `MembersV1` +
    // `MemberInfoV1` for 200 members is dominated by keys and signatures, so
    // the totals are stable for fixed seeds.
    assert_eq!(signatures.len(), 400);
    assert!(
        saved > 22_000,
        "expected >22 KB saved across 400 signatures, got {saved}"
    );
}
