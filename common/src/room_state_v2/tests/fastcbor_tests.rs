//! The v1 codec against ciborium: on every legacy record kind, both encode
//! to the same bytes and both decode the same bytes to the same value, and
//! the codec refuses the non-canonical forms ciborium's re-encode check
//! refuses. And every signature check of a legacy record gives the
//! ciborium path's answer, and a non-canonical record whose signed field is
//! intact is still refused.

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

/// The end of the CBOR item at `i` of canonical `b` (definite lengths;
/// integers, strings, arrays, maps and simple values only).
fn item_end(b: &[u8], mut i: usize) -> usize {
    let mut left = 1u64;
    while left > 0 {
        left -= 1;
        let ib = b[i];
        i += 1;
        let (major, info) = (ib >> 5, ib & 31);
        if major == 7 {
            continue;
        }
        let a = match info {
            0..=23 => info as u64,
            24..=27 => {
                let n = 1usize << (info - 24);
                let v = b[i..i + n].iter().fold(0u64, |a, &x| (a << 8) | x as u64);
                i += n;
                v
            }
            _ => panic!("indefinite"),
        };
        match major {
            0 | 1 => {}
            2 | 3 => i += a as usize,
            4 => left += a,
            5 => left += 2 * a,
            _ => panic!("tag"),
        }
    }
    i
}

/// The entries of a top-level map of up to 23 entries (every v1 authorized
/// struct): each entry's key-and-value byte range.
fn entries(bytes: &[u8]) -> Vec<std::ops::Range<usize>> {
    assert_eq!(bytes[0] >> 5, 5, "a map");
    let n = (bytes[0] & 31) as usize;
    assert!(n < 24);
    let mut out = Vec::new();
    let mut i = 1;
    for _ in 0..n {
        let k = item_end(bytes, i);
        let v = item_end(bytes, k);
        out.push(i..v);
        i = v;
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
    Some(v1.expect("decode keeps the signature").verify(o, &l.signer))
}

/// Checks one legacy record's signature paths against the ciborium path:
/// `legacy::verify` and the check `decode_*` keeps give its answer, and
/// swapping two map entries (a form serde decodes, with the signed field
/// intact) is refused by every path. Returns whether the signature
/// verifies.
pub fn check_signed(o: &Key, r: &Record) -> bool {
    use crate::room_state_v2::legacy;
    let (l, old, lenient) = match r {
        Record::LegacyConfiguration(l) => (
            l,
            old_verify::<AuthorizedConfigurationV1, _>(l, |a| (&a.configuration, &a.signature)),
            lenient::<AuthorizedConfigurationV1> as fn(&[u8]) -> bool,
        ),
        Record::LegacyLink(l) => (
            l,
            old_verify::<AuthorizedMember, _>(l, |a| (&a.member, &a.signature)),
            lenient::<AuthorizedMember> as fn(&[u8]) -> bool,
        ),
        Record::LegacyBan(l) => (
            l,
            old_verify::<AuthorizedUserBan, _>(l, |a| (&a.ban, &a.signature)),
            lenient::<AuthorizedUserBan> as fn(&[u8]) -> bool,
        ),
        Record::LegacyActivity(l) | Record::LegacyMessage(l) => (
            l,
            old_verify::<AuthorizedMessageV1, _>(l, |a| (&a.message, &a.signature)),
            lenient::<AuthorizedMessageV1> as fn(&[u8]) -> bool,
        ),
        Record::LegacyMemberInfo(l) => (
            l,
            old_verify::<AuthorizedMemberInfo, _>(l, |a| (&a.member_info, &a.signature)),
            lenient::<AuthorizedMemberInfo> as fn(&[u8]) -> bool,
        ),
        Record::LegacySecretBlob(l) => (
            l,
            old_verify::<AuthorizedEncryptedSecretForMember, _>(l, |a| {
                (&a.secret, &a.owner_signature)
            }),
            lenient::<AuthorizedEncryptedSecretForMember> as fn(&[u8]) -> bool,
        ),
        // Both paths build their signed bytes with the same function: only
        // the refusal of non-canonical forms is checked here.
        Record::LegacyDirectMessage(l) => (
            l,
            legacy::verify(o, r),
            lenient::<AuthorizedDirectMessage> as fn(&[u8]) -> bool,
        ),
        Record::LegacyPurge(l) => (
            l,
            legacy::verify(o, r),
            lenient::<AuthorizedRecipientPurges> as fn(&[u8]) -> bool,
        ),
        _ => panic!("not a legacy record"),
    };
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

// ------------------------------------------------------- hostile input

/// A type that nests through a known field, for the depth limit.
#[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
struct Deep(Vec<Deep>);

/// `n` arrays nested in each other, the innermost empty.
fn nested(n: usize) -> Vec<u8> {
    let mut b = vec![0x81; n - 1];
    b.push(0x80);
    b
}

/// One record per legacy kind, from the generated state.
fn one_per_kind() -> Vec<Record> {
    let (s, dropped) = largest_state(
        Caps {
            max_secret_versions: 2,
            ..Caps::ceilings_legacy().scaled(4)
        },
        0,
    );
    assert_eq!(dropped, 0);
    let mut seen = std::collections::BTreeSet::new();
    s.records
        .into_iter()
        .filter(|r| check_kind(r).is_some() && seen.insert(r.kind()))
        .collect()
}

fn check_kind(r: &Record) -> Option<u8> {
    matches!(
        r,
        Record::LegacyConfiguration(_)
            | Record::LegacyLink(_)
            | Record::LegacyBan(_)
            | Record::LegacyActivity(_)
            | Record::LegacyMessage(_)
            | Record::LegacyMemberInfo(_)
            | Record::LegacyDirectMessage(_)
            | Record::LegacyPurge(_)
            | Record::LegacySecretBlob(_)
    )
    .then(|| r.kind())
}

/// `bytes` (a top-level map of under 23 entries) with one more entry, the
/// key "x" and a value of `depth` nested arrays.
fn with_nested_unknown(bytes: &[u8], depth: usize) -> Vec<u8> {
    assert_eq!(bytes[0] >> 5, 5);
    let n = bytes[0] & 31;
    assert!(n < 23);
    let mut b = vec![bytes[0] + 1];
    b.extend_from_slice(&bytes[1..]);
    b.extend_from_slice(&[0x61, b'x']);
    b.extend(std::iter::repeat_n(0x81, depth));
    b.push(0x80);
    b
}

fn kinds_agree_on(b: &[u8], r: &Record) -> (bool, bool) {
    macro_rules! both {
        ($t:ty) => {
            (
                fastcbor::from_canonical::<$t>(b).is_some(),
                from_cbor_canonical::<$t>(b).is_some(),
            )
        };
    }
    match r {
        Record::LegacyConfiguration(_) => both!(AuthorizedConfigurationV1),
        Record::LegacyLink(_) => both!(AuthorizedMember),
        Record::LegacyBan(_) => both!(AuthorizedUserBan),
        Record::LegacyActivity(_) | Record::LegacyMessage(_) => both!(AuthorizedMessageV1),
        Record::LegacyMemberInfo(_) => both!(AuthorizedMemberInfo),
        Record::LegacyDirectMessage(_) => both!(AuthorizedDirectMessage),
        Record::LegacyPurge(_) => both!(AuthorizedRecipientPurges),
        Record::LegacySecretBlob(_) => both!(AuthorizedEncryptedSecretForMember),
        _ => unreachable!(),
    }
}

/// A deeply nested unknown field is refused, without a crash, on every
/// kind. ciborium's recursion limit (256) stops its own walk, so its path
/// is run too.
#[test]
fn fastcbor_refuses_a_deeply_nested_unknown_field() {
    for r in one_per_kind() {
        let v1 = with_bytes(&r);
        for depth in [1, 255, 256, 257, 100_000] {
            let b = with_nested_unknown(&v1, depth);
            let (fast, old) = kinds_agree_on(&b, &r);
            assert!(!fast, "kind {} depth {depth}: fastcbor accepts", r.kind());
            assert!(!old, "kind {} depth {depth}: ciborium accepts", r.kind());
        }
    }
}

/// Nesting through a known field is refused past 256 levels, and the
/// boundary is ciborium's.
#[test]
fn fastcbor_depth_limit_matches_ciborium() {
    for n in [1, 2, 255, 256, 257, 258, 1000, 100_000] {
        let b = nested(n);
        let fast = fastcbor::from_canonical::<Deep>(&b).is_some();
        let old = from_cbor_canonical::<Deep>(&b).is_some();
        assert_eq!(fast, old, "depth {n}: fastcbor {fast}, ciborium {old}");
        if n > 258 {
            assert!(!fast, "depth {n} accepted");
        }
    }
    assert!(fastcbor::from_canonical::<Deep>(&nested(256)).is_some());
}

// ------------------------------------------------- malformed-input sweep

/// Fixed-seed generator (xorshift64).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The start and major type of every item in canonical `b`.
fn item_starts(b: &[u8]) -> Vec<(usize, u8)> {
    let mut out = Vec::new();
    let (mut i, mut left) = (0usize, 1u64);
    while left > 0 {
        left -= 1;
        let (start, ib) = (i, b[i]);
        i += 1;
        let (major, info) = (ib >> 5, ib & 31);
        out.push((start, major));
        if major == 7 {
            continue;
        }
        let a = match info {
            0..=23 => info as u64,
            24..=27 => {
                let n = 1usize << (info - 24);
                let v = b[i..i + n].iter().fold(0u64, |a, &x| (a << 8) | x as u64);
                i += n;
                v
            }
            _ => panic!("indefinite"),
        };
        match major {
            2 | 3 => i += a as usize,
            4 => left += a,
            5 => left += 2 * a,
            _ => {}
        }
    }
    out
}

/// A CBOR header, minimal or padded to `width` argument bytes (0 = minimal).
fn header(major: u8, v: u64, width: usize) -> Vec<u8> {
    let m = major << 5;
    let w = if width != 0 {
        width
    } else if v < 24 {
        0
    } else if v <= 0xff {
        1
    } else if v <= 0xffff {
        2
    } else if v <= 0xffff_ffff {
        4
    } else {
        8
    };
    if w == 0 {
        return vec![m | v as u8];
    }
    let mut h = vec![m | (24 + w.trailing_zeros() as u8)];
    h.extend_from_slice(&v.to_be_bytes()[8 - w..]);
    h
}

/// The byte length of the header at `b[0]`.
fn header_len(b: u8) -> usize {
    match b & 31 {
        0..=23 => 1,
        x => 1 + (1 << (x - 24)),
    }
}

/// Deterministic malformed variants of canonical `b`, `positions` of each
/// positional kind.
fn mutations(b: &[u8], positions: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut rng = Rng(seed | 1);
    let mut out = Vec::new();
    let starts = item_starts(b);
    // Every single-byte flip, at sampled positions.
    for _ in 0..positions {
        let p = rng.below(b.len());
        let mut m = b.to_vec();
        m[p] ^= 1 << rng.below(8);
        out.push(m);
        let mut m = b.to_vec();
        m[p] = rng.next() as u8;
        out.push(m);
    }
    // Truncations and an appended byte.
    for k in [0, 1, 2, b.len() / 2, b.len() - 1] {
        out.push(b[..k].to_vec());
    }
    for x in [0u8, 0x80, 0xff] {
        let mut m = b.to_vec();
        m.push(x);
        out.push(m);
    }
    // Per sampled item (one in ten of `positions`): a length made larger than the buffer, a header made
    // non-minimal, an indefinite marker, and boundary integers.
    const BOUNDS: [u64; 14] = [
        0,
        23,
        24,
        255,
        256,
        65535,
        65536,
        u32::MAX as u64,
        1 << 32,
        i64::MAX as u64,
        i64::MAX as u64 + 1,
        u64::MAX,
        1,
        22,
    ];
    for _ in 0..(positions / 10).min(starts.len()) {
        let (p, major) = starts[rng.below(starts.len())];
        let hl = header_len(b[p]);
        let splice = |h: &[u8]| {
            let mut m = b[..p].to_vec();
            m.extend_from_slice(h);
            m.extend_from_slice(&b[p + hl..]);
            m
        };
        if (2..=5).contains(&major) {
            out.push(splice(&header(major, b.len() as u64 + 1, 0)));
            out.push(splice(&header(major, u32::MAX as u64 + 1, 0)));
            out.push(splice(&header(major, u64::MAX, 0)));
            out.push(splice(&[(major << 5) | 31]));
        }
        if major < 7 {
            let v = if hl == 1 {
                (b[p] & 31) as u64
            } else {
                b[p + 1..p + hl]
                    .iter()
                    .fold(0u64, |a, &x| (a << 8) | x as u64)
            };
            for w in [1, 2, 4, 8] {
                if hl != 1 + w {
                    out.push(splice(&header(major, v, w)));
                }
            }
        }
        if major < 2 {
            for v in BOUNDS {
                out.push(splice(&header(major, v, 0)));
            }
        }
    }
    // A duplicated map key (the first entry's key over the second's, and
    // the first entry appended again).
    if b[0] >> 5 == 5 && (b[0] & 31) >= 2 && (b[0] & 31) < 23 {
        let e = entries(b);
        let k0 = item_end(b, e[0].start);
        let k1 = item_end(b, e[1].start);
        let mut m = b[..e[1].start].to_vec();
        m.extend_from_slice(&b[e[0].start..k0]);
        m.extend_from_slice(&b[k1..]);
        out.push(m);
        let mut m = b.to_vec();
        m[0] += 1;
        m.extend_from_slice(&b[e[0].clone()]);
        out.push(m);
    }
    out
}

fn show(b: &[u8]) -> String {
    b.iter().take(64).map(|x| format!("{x:02x}")).collect()
}

fn agree<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(b: &[u8]) {
    let f = fastcbor::from_canonical::<T>(b);
    let c = from_cbor_canonical::<T>(b);
    match (f, c) {
        (None, None) => {}
        (Some(f), Some(c)) => assert_eq!(f, c, "decoded values differ on {}", show(b)),
        (f, c) => panic!(
            "decoders disagree on {} ({} bytes): fastcbor accepts {}, ciborium accepts {}",
            show(b),
            b.len(),
            f.is_some(),
            c.is_some()
        ),
    }
}

fn agree_record(r: &Record, b: &[u8]) {
    match r {
        Record::LegacyConfiguration(_) => agree::<AuthorizedConfigurationV1>(b),
        Record::LegacyLink(_) => agree::<AuthorizedMember>(b),
        Record::LegacyBan(_) => agree::<AuthorizedUserBan>(b),
        Record::LegacyActivity(_) | Record::LegacyMessage(_) => agree::<AuthorizedMessageV1>(b),
        Record::LegacyMemberInfo(_) => agree::<AuthorizedMemberInfo>(b),
        Record::LegacyDirectMessage(_) => agree::<AuthorizedDirectMessage>(b),
        Record::LegacyPurge(_) => agree::<AuthorizedRecipientPurges>(b),
        Record::LegacySecretBlob(_) => agree::<AuthorizedEncryptedSecretForMember>(b),
        _ => unreachable!(),
    }
}

/// Runs the sweep over up to `per_kind` records of each legacy kind.
fn sweep(per_kind: usize, positions: usize) -> usize {
    let mut taken = std::collections::BTreeMap::<u8, usize>::new();
    let mut cases = 0;
    for secret in [0, 2] {
        let (s, dropped) = largest_state(
            Caps {
                max_secret_versions: secret,
                ..Caps::ceilings_legacy().scaled(4)
            },
            0,
        );
        assert_eq!(dropped, 0);
        for r in &s.records {
            if check_kind(r).is_none() {
                continue;
            }
            let t = taken.entry(r.kind() + 100 * secret as u8).or_default();
            if *t >= per_kind {
                continue;
            }
            *t += 1;
            let v1 = with_bytes(r);
            agree_record(r, &v1);
            for m in mutations(&v1, positions, 0x9e37_79b9 + *t as u64) {
                agree_record(r, &m);
                cases += 1;
            }
        }
    }
    cases
}

/// A fixed-seed sample of malformed records: both decoders accept and
/// refuse the same ones, and decode the same value when they accept.
#[test]
fn fastcbor_agrees_with_ciborium_on_malformed_records() {
    let cases = sweep(1, 200);
    assert!(cases > 3000, "only {cases} cases");
}

/// The same sweep over more records and positions.
#[test]
#[ignore = "slow"]
fn fastcbor_agrees_with_ciborium_on_malformed_records_large() {
    let cases = sweep(40, 2000);
    println!("{cases} cases");
}
