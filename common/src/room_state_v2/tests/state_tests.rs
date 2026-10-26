//! `validate_state`, summaries, deltas, the contract-boundary signature
//! rule, encoding determinism, and the size bounds.

use crate::room_state_v2::bench::{largest_state, owner_key, Caps};
use crate::room_state_v2::config::ceiling;
use crate::room_state_v2::encoding::{to_cbor, Bytes32, Key, Sig};
use crate::room_state_v2::records::{Record, Signed};
use crate::room_state_v2::state::{
    apply_delta, max_state_bytes, merge, validate_state, Invalid, RoomStateV2, MAX_RECORD_BYTES,
};
use crate::room_state_v2::Verifier;

fn owner() -> Key {
    Key::from(&owner_key().verifying_key())
}

fn small(v: u8) -> RoomStateV2 {
    let (s, dropped) = largest_state(Caps::ceilings().scaled(400), v);
    assert_eq!(
        dropped, 0,
        "the largest-state builder built records f drops"
    );
    s
}

#[test]
fn the_largest_state_builder_builds_valid_states() {
    let o = owner();
    for v in [0, 1] {
        let s = small(v);
        let ok = validate_state(&o, &s.to_bytes(), &mut Verifier::new()).unwrap();
        assert_eq!(ok, s);
    }
}

#[test]
fn merge_of_two_large_states_is_valid_and_commutative() {
    let o = owner();
    let (a, b) = (small(0), small(1));
    let ab = merge(&o, &a, &b, &mut Verifier::new());
    let ba = merge(&o, &b, &a, &mut Verifier::new());
    assert_eq!(ab, ba);
    assert!(validate_state(&o, &ab.to_bytes(), &mut Verifier::new()).is_ok());
    assert_eq!(merge(&o, &ab, &a, &mut Verifier::new()), ab);
    assert_eq!(merge(&o, &ab, &b, &mut Verifier::new()), ab);
    assert!(ab.records.len() > a.records.len(), "the union competes");
}

#[test]
fn validate_refuses_what_f_would_change() {
    let o = owner();
    let s = small(0);
    // Unsorted.
    let mut t = s.clone();
    t.records.swap(0, 1);
    assert_eq!(
        validate_state(&o, &t.to_bytes(), &mut Verifier::new()),
        Err(Invalid::NotSortedUnique)
    );
    // Duplicate.
    let mut t = s.clone();
    t.records.insert(1, t.records[0].clone());
    assert_eq!(
        validate_state(&o, &t.to_bytes(), &mut Verifier::new()),
        Err(Invalid::NotSortedUnique)
    );
    // Wrong version.
    let mut t = s.clone();
    t.version = 1;
    assert_eq!(
        validate_state(&o, &t.to_bytes(), &mut Verifier::new()),
        Err(Invalid::WrongVersion(1))
    );
    // A record with a broken signature is not a fixpoint (f drops it).
    let mut t = s.clone();
    let i = t
        .records
        .iter()
        .position(|r| matches!(r, Record::Message(_)))
        .unwrap();
    if let Record::Message(m) = &mut t.records[i] {
        m.sig = Sig([0; 64]);
    }
    let t = RoomStateV2::from_records(t.records);
    assert_eq!(
        validate_state(&o, &t.to_bytes(), &mut Verifier::new()),
        Err(Invalid::NotAFixpoint(1))
    );
    // Non-canonical bytes.
    let mut bytes = s.to_bytes();
    bytes.push(0);
    assert_eq!(
        validate_state(&o, &bytes, &mut Verifier::new()),
        Err(Invalid::NotCanonical)
    );
    // Another room's owner: nothing verifies.
    let other = Bytes32([9; 32]);
    assert!(validate_state(&other, &s.to_bytes(), &mut Verifier::new()).is_err());
}

#[test]
fn delta_against_a_summary_reconciles_in_one_exchange() {
    let o = owner();
    let (a, b) = (small(0), small(1));
    let merged = merge(&o, &a, &b, &mut Verifier::new());
    // a sends to b what b lacks, and b to a.
    let da = a.delta(&b.summarize()).unwrap();
    let db = b.delta(&a.summarize()).unwrap();
    let b2 = apply_delta(&o, &b, da, &mut Verifier::new()).unwrap();
    let a2 = apply_delta(&o, &a, db, &mut Verifier::new()).unwrap();
    assert_eq!(a2, merged);
    assert_eq!(b2, merged);
    // Nothing to send between equal states.
    assert_eq!(a2.delta(&b2.summarize()), None);
    // Applying the same delta twice changes nothing.
    let again = apply_delta(&o, &a2, b.records.clone(), &mut Verifier::new()).unwrap();
    assert_eq!(again, a2);
}

#[test]
fn a_delta_with_a_bad_signature_is_refused() {
    let o = owner();
    let (a, b) = (small(0), small(1));
    let mut d = a.delta(&b.summarize()).unwrap();
    let i = d
        .iter()
        .position(|r| matches!(r, Record::Activity(_)))
        .unwrap();
    if let Record::Activity(s) = &mut d[i] {
        s.sig = Sig([1; 64]);
    }
    assert!(matches!(
        apply_delta(&o, &b, d, &mut Verifier::new()),
        Err(Invalid::BadSignature(_))
    ));
}

#[test]
fn summaries_serialize_deterministically() {
    let s = small(0);
    let mut rev = s.clone();
    rev.records.reverse();
    // The summary type orders by kind and digest whatever the record order.
    assert_eq!(to_cbor(&s.summarize()), to_cbor(&rev.summarize()));
}

#[test]
fn record_digest_golden_vector() {
    // One fixed record, its fixed encoding and its fixed digest: the wire
    // form and the identity every summary, delta and link carries.
    let r = Record::Clock(Signed {
        payload: crate::room_state_v2::records::ClockP { hours: 7 },
        sig: Sig([0x11; 64]),
    });
    let enc = to_cbor(&r);
    // [2, [[7], h'11..11']]
    let mut expected = vec![0x82, 0x02, 0x82, 0x81, 0x07, 0x58, 64];
    expected.extend([0x11; 64]);
    assert_eq!(enc, expected);
    assert_eq!(
        r.digest(),
        Bytes32(*blake3::hash(&expected).as_bytes()),
        "the digest is BLAKE3-256 of the canonical bytes"
    );
    let hex: String = r.digest().0.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, GOLDEN_CLOCK_DIGEST);
}

const GOLDEN_CLOCK_DIGEST: &str =
    "69a05020f977abf4ed4255f7bdda08a6add7de343b58c34bd1719469473b93e7";

/// The largest valid states at the ceilings (all v2 records, and the
/// member-dependent records as legacy v1 ones): each is a fixpoint, within
/// the computed byte ceiling and freenet-core's 50 MiB state limit, with
/// every record within `MAX_RECORD_BYTES`; and the merge of two is valid.
#[test]
fn largest_state_at_the_ceilings() {
    let o = owner();
    for caps in [Caps::ceilings(), Caps::ceilings_legacy()] {
        let (s, dropped) = largest_state(caps, 0);
        assert_eq!(dropped, 0, "{caps:?}");
        let (s1, _) = largest_state(caps, 1);
        let bytes = s.to_bytes();
        let mut per_kind: std::collections::BTreeMap<u8, (usize, usize, usize)> =
            Default::default();
        for r in &s.records {
            let n = to_cbor(r).len();
            let e = per_kind.entry(r.kind()).or_default();
            e.0 += 1;
            e.1 += n;
            e.2 = e.2.max(n);
        }
        println!("legacy {}:", caps.legacy);
        for (k, (n, total, max)) in &per_kind {
            println!("  kind {k:2}: {n:5} records, {total:9} bytes, largest {max}");
            assert!(*max <= MAX_RECORD_BYTES[*k as usize], "kind {k}: {max}");
        }
        println!(
            "  state {} bytes; computed ceiling {}",
            bytes.len(),
            max_state_bytes()
        );
        assert!(bytes.len() <= max_state_bytes());
        assert!(max_state_bytes() <= 50 * 1024 * 1024, "over MAX_STATE_SIZE");
        let mut v = Verifier::new();
        assert!(validate_state(&o, &bytes, &mut v).is_ok());
        let m = merge(&o, &s, &s1, &mut Verifier::new());
        assert!(validate_state(&o, &m.to_bytes(), &mut Verifier::new()).is_ok());
        let _ = ceiling::MAX_LINKS;
    }
}

/// The live-state conversion report: reads the v1 state at RIVER_V1_STATE
/// (CBOR, as a node's GET returns it), converts it both ways (the owner
/// converting first, with its records trusted since its key is not here,
/// and the owner never converting), and prints what survives `f`.
#[test]
#[ignore = "reads RIVER_V1_STATE; run by hand against a fetched live state"]
fn convert_live_v1_state() {
    use crate::room_state::ChatRoomStateV1;
    use crate::room_state_v2::convert::{legacy_configuration, legacy_records, owner_payloads};
    use crate::room_state_v2::legacy;
    use crate::room_state_v2::retain::retain;
    use std::collections::BTreeMap;
    let path = std::env::var("RIVER_V1_STATE").unwrap();
    let v1: ChatRoomStateV1 = ciborium::de::from_reader(&std::fs::read(path).unwrap()[..]).unwrap();
    let o = Key::from(&v1_owner(&v1));
    let conv = legacy_records(&o, &v1);
    println!("v1: {} members, {} bans, {} member info, {} messages, {} DMs, {} purges, {} blobs, {} secret versions, {} evidence",
        v1.members.members.len(), v1.bans.0.len(), v1.member_info.member_info.len(),
        v1.recent_messages.messages.len(), v1.direct_messages.messages.len(),
        v1.direct_messages.purges.len(), v1.secrets.encrypted_secrets.len(),
        v1.secrets.versions.len(), v1.ban_evidence.members.len());
    println!(
        "legacy records: {}; unconverted (no signer key): {:?}",
        conv.records.len(),
        conv.unconverted
    );
    let strict_fail: Vec<u8> = conv
        .records
        .iter()
        .filter(|r| !legacy::verify(&o, r))
        .map(|r| r.kind())
        .collect();
    println!("verify_strict failures by kind: {strict_fail:?}");
    let v1_members: Vec<Key> = v1
        .members
        .members
        .iter()
        .map(|m| Key::from(&m.member.member_vk))
        .collect();
    let report = |label: &str, owner_recs: Vec<Record>, trusted: bool| {
        let n = owner_recs.len();
        let mut all = owner_recs;
        all.extend(conv.records.iter().cloned());
        let mut v = Verifier::new();
        if trusted {
            v.trust(all[..n].iter().map(|r| r.digest()));
        }
        let r = retain(&o, &all, &mut v);
        let mut tally: BTreeMap<u8, (usize, usize)> = BTreeMap::new();
        for (rec, k) in all.iter().zip(&r.keep) {
            let e = tally.entry(rec.kind()).or_default();
            e.0 += 1;
            e.1 += *k as usize;
        }
        let lost: Vec<&Key> = v1_members
            .iter()
            .filter(|k| r.members.binary_search(k).is_err())
            .collect();
        println!(
            "== {label}: {} members kept, {} v1 members lost",
            r.members.len(),
            lost.len()
        );
        for (k, (n, kept)) in tally {
            println!(
                "   kind {k:2}: {n:4} in, {kept:4} kept, {:4} dropped",
                n - kept
            );
        }
        println!(
            "   deputies {}; config {:?}",
            r.config.deputies.len(),
            (
                r.config.max_members,
                r.config.max_owner_bans,
                r.config.max_deputy_bans,
                r.config.max_bans_per_deputy,
                r.config.max_member_bans
            )
        );
        (r, all, lost.len())
    };
    let clock = ((std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - crate::room_state_v2::config::EPOCH_2026_SECS)
        / 3600) as u32;
    let p = owner_payloads(&o, &v1, &conv.records, clock);
    let stand_in = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32]);
    let (_, _, lost_a) = report(
        "owner converts (owner records trusted)",
        p.sign(&stand_in),
        true,
    );
    let (r, all, lost_b) = report(
        "owner never converts (v1 configuration, window open)",
        vec![legacy_configuration(&o, &v1)],
        false,
    );
    // The dropped records of the second path, by signer.
    let members: std::collections::HashSet<Key> = r.members.iter().copied().collect();
    let mut by_signer: BTreeMap<(u8, String), usize> = BTreeMap::new();
    for (rec, k) in all.iter().zip(&r.keep) {
        if *k {
            continue;
        }
        let signer = match rec {
            Record::LegacyBan(l) | Record::LegacyActivity(l) | Record::LegacyMemberInfo(l) => {
                l.signer
            }
            _ => continue,
        };
        let who = if signer == o {
            "owner".to_string()
        } else if members.contains(&signer) {
            format!("member {:?}", signer)
        } else {
            "not a member".to_string()
        };
        *by_signer.entry((rec.kind(), who)).or_default() += 1;
    }
    for ((k, who), n) in by_signer {
        println!("   dropped kind {k}: {n} signed by {who}");
    }
    assert_eq!((lost_a, lost_b), (0, 0), "v1 members lost");
}

fn v1_owner(v1: &crate::room_state::ChatRoomStateV1) -> ed25519_dalek::VerifyingKey {
    // The owner key is the room's contract parameter; the state names only its
    // 64-bit id, so take it from RIVER_V1_OWNER (base58).
    let _ = v1;
    let b = bs58::decode(std::env::var("RIVER_V1_OWNER").unwrap())
        .into_vec()
        .unwrap();
    ed25519_dalek::VerifyingKey::from_bytes(&b.try_into().unwrap()).unwrap()
}
