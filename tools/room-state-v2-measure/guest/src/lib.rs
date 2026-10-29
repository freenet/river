//! The v2 entry points the room contract will call, compiled to WASM with
//! the room contract's release profile. Each takes the owner key and CBOR
//! state bytes from guest memory the host filled through `alloc`.

use river_core::room_state_v2::encoding::{from_cbor_canonical, Bytes32};
use river_core::room_state_v2::{apply_delta, normalize, validate_state, RoomStateV2, Verifier};
use std::cell::Cell;
use std::collections::HashSet;

thread_local! {
    static CHECKS: Cell<i64> = const { Cell::new(0) };
}

fn slice<'a>(p: *const u8, n: usize) -> &'a [u8] {
    unsafe { std::slice::from_raw_parts(p, n) }
}

fn owner(p: *const u8) -> Bytes32 {
    Bytes32(slice(p, 32).try_into().unwrap())
}

fn decode(p: *const u8, n: usize) -> Option<RoomStateV2> {
    from_cbor_canonical(slice(p, n))
}

#[no_mangle]
pub extern "C" fn alloc(n: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(n);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// Signature checks run by the last call.
#[no_mangle]
pub extern "C" fn checks() -> i64 {
    CHECKS.with(|c| c.get())
}

/// `validate_state`: the record count, or -1 when refused.
#[no_mangle]
pub extern "C" fn validate(o: *const u8, p: *const u8, n: usize) -> i64 {
    let mut v = Verifier::new();
    let r = match validate_state(&owner(o), slice(p, n), &mut v) {
        Ok(s) => s.records.len() as i64,
        Err(_) => -1,
    };
    CHECKS.with(|c| c.set(v.checks as i64));
    r
}

/// `f(A ∪ B)` with neither input trusted (every signature `f` keeps is
/// checked): the merged record count.
#[no_mangle]
pub extern "C" fn merge_untrusted(
    o: *const u8,
    a: *const u8,
    an: usize,
    b: *const u8,
    bn: usize,
) -> i64 {
    let (Some(a), Some(b)) = (decode(a, an), decode(b, bn)) else {
        return -1;
    };
    let mut v = Verifier::new();
    let mut all = a.records;
    all.extend(b.records);
    let (s, _) = normalize(&owner(o), all, &mut v);
    CHECKS.with(|c| c.set(v.checks as i64));
    s.records.len() as i64
}

/// `update_state` with a whole state: `a` is the held (validated) state,
/// `b`'s records it lacks are the delta, every one of them checked.
#[no_mangle]
pub extern "C" fn update_with_state(
    o: *const u8,
    a: *const u8,
    an: usize,
    b: *const u8,
    bn: usize,
) -> i64 {
    let (Some(a), Some(b)) = (decode(a, an), decode(b, bn)) else {
        return -1;
    };
    let held: HashSet<_> = a.digests().into_iter().collect();
    let delta: Vec<_> = b
        .records
        .into_iter()
        .filter(|r| !held.contains(&r.digest()))
        .collect();
    let mut v = Verifier::new();
    let r = match apply_delta(&owner(o), &a, delta, &mut v) {
        Ok(s) => s.records.len() as i64,
        Err(_) => -2,
    };
    CHECKS.with(|c| c.set(v.checks as i64));
    r
}

/// `summarize_state`: the summary's encoded length.
#[no_mangle]
pub extern "C" fn summarize(p: *const u8, n: usize) -> i64 {
    match decode(p, n) {
        Some(s) => river_core::room_state_v2::encoding::to_cbor(&s.summarize()).len() as i64,
        None => -1,
    }
}

/// Verifies one fixed signature `n` times (the per-signature cost).
#[no_mangle]
pub extern "C" fn verify_n(n: i32) -> i64 {
    use river_core::room_state_v2::encoding::{sign_payload, verify_payload};
    let sk = river_core::room_state_v2::bench::owner_key();
    let o = Bytes32(sk.verifying_key().to_bytes());
    let msg = [7u8; 100];
    let sig = sign_payload(1, &o, &msg[..], &sk);
    let mut ok = 0;
    for _ in 0..n {
        ok += verify_payload(1, &o, &msg[..], &o, &sig) as i64;
    }
    ok
}

/// BLAKE3 over `n` bytes of guest memory (the per-byte digest cost).
#[no_mangle]
pub extern "C" fn hash_n(p: *const u8, n: usize) -> i64 {
    blake3::hash(slice(p, n)).as_bytes()[0] as i64
}

/// `validate_state` with every signature taken as verified: the cost of
/// everything but signatures.
#[no_mangle]
pub extern "C" fn validate_no_signatures(o: *const u8, p: *const u8, n: usize) -> i64 {
    let Some(s) = decode(p, n) else { return -1 };
    let mut v = Verifier::new();
    v.trust(&owner(o), s.digests());
    let r = match validate_state(&owner(o), slice(p, n), &mut v) {
        Ok(s) => s.records.len() as i64,
        Err(_) => -1,
    };
    CHECKS.with(|c| c.set(v.checks as i64));
    r
}
