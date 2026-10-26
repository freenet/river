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

/// The largest state at the placeholder ceilings: its size against the
/// computed ceiling, and per-kind record sizes against `MAX_RECORD_BYTES`.
#[test]
#[ignore = "builds ~150k signed records (about a minute in the dev profile, which optimizes the crypto crates)"]
fn largest_state_at_the_ceilings() {
    let o = owner();
    let t = std::time::Instant::now();
    let (s, dropped) = largest_state(Caps::ceilings(), 0);
    println!("built in {:?}", t.elapsed());
    assert_eq!(dropped, 0);
    let bytes = s.to_bytes();
    let mut per_kind: std::collections::BTreeMap<u8, (usize, usize, usize)> = Default::default();
    for r in &s.records {
        let n = to_cbor(r).len();
        let e = per_kind.entry(r.kind()).or_default();
        e.0 += 1;
        e.1 += n;
        e.2 = e.2.max(n);
    }
    for (k, (n, total, max)) in &per_kind {
        println!("kind {k:2}: {n:6} records, {total:10} bytes, largest {max}");
        assert!(*max <= MAX_RECORD_BYTES[*k as usize], "kind {k}: {max}");
    }
    println!(
        "state {} bytes; ceiling {}; freenet-core MAX_STATE_SIZE {}",
        bytes.len(),
        max_state_bytes(),
        50 * 1024 * 1024
    );
    assert!(bytes.len() <= max_state_bytes());
    let t = std::time::Instant::now();
    let mut v = Verifier::new();
    validate_state(&o, &bytes, &mut v).unwrap();
    println!(
        "native validate {:?}, {} signature checks",
        t.elapsed(),
        v.checks
    );
    let _ = ceiling::MAX_LINKS;
}
