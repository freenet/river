//! Golden pins of `f` on the tiny families: a hash of each family's whole
//! table (every subset, and what `f` keeps of it), keyed by record names so
//! it does not depend on record order. The law suites cannot see a rule
//! that changes what `f` keeps while keeping it lawful (a cap removed, an
//! ordering changed); these pins can. They are pins, not a differential
//! test: the executable model has diverged from the real `f` (the decisions
//! in `mod.rs` since it was written), so each pinned table was read against
//! the decisions rather than computed by the model. A change to `f` that is
//! meant moves a pin: print the tables (`RIVER_V2_PRINT_PINS=1`), check the
//! differences against the change, and update the hash.

use super::families::{legacy_families, tiny_families};
use super::harness::{bits, LawModel, S};
use super::universe::Universe;
use crate::room_state_v2::retain::mutant::Mutant;

/// The table's hash, with each record named by its rank among the sorted
/// names (so builder order does not matter).
fn table_hash(m: &Universe) -> (String, Vec<String>) {
    let n = m.n();
    let mut names: Vec<(String, usize)> = m
        .names
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, s)| (s, i))
        .collect();
    names.sort();
    let mut rank = vec![0usize; n];
    for (r, (_, i)) in names.iter().enumerate() {
        rank[*i] = r;
    }
    let canon = |s: S| bits(s).fold(0u128, |a, i| a | (1u128 << rank[i]));
    let mut rows: Vec<(u128, u128)> = (0..(1u128 << n))
        .map(|s| (canon(s), canon(m.f(s))))
        .collect();
    rows.sort();
    let mut h = blake3::Hasher::new();
    for (name, _) in &names {
        h.update(name.as_bytes());
        h.update(&[0]);
    }
    for (s, k) in &rows {
        h.update(&s.to_le_bytes());
        h.update(&k.to_le_bytes());
    }
    let full = || {
        let all: S = (1u128 << n) - 1;
        bits(m.f(all)).map(|i| m.names[i].clone()).collect()
    };
    (h.finalize().to_hex()[..16].to_string(), full())
}

/// (family, configurations index among families of that name, hash).
const PINS: &[(&str, usize, &str)] = &[
    ("U1 checkpoint", 0, "8f1021b353fd4b19"),
    ("U1 checkpoint", 1, "2cb15e28172126c6"),
    ("U2 quotas full", 0, "a466aca6d91676ca"),
    ("U2 quotas full", 1, "97fab908fb9b936a"),
    ("U3 owner+deputy bans", 0, "5442a1c73a93f01e"),
    ("U3 owner+deputy bans", 1, "c2ec359be2702daa"),
    ("U4 member bans+clock", 0, "b1eeea63e1d83995"),
    ("U5 cap+DM", 0, "5844882faddda4d6"),
    ("U6 legacy", 0, "e1a2709b0312c32f"),
    ("U6 legacy", 1, "e1a2709b0312c32f"),
    ("U7 legacy chain", 0, "0f6311e57408d63d"),
    ("U7 legacy chain", 1, "a9a2818d9297d2ab"),
];

#[test]
fn f_on_the_tiny_families_matches_its_pins() {
    let print = std::env::var("RIVER_V2_PRINT_PINS").is_ok();
    let mut seen: std::collections::HashMap<&str, usize> = Default::default();
    let mut got = vec![];
    for (name, ub, base, v1) in tiny_families().into_iter().chain(legacy_families()) {
        let n = seen.entry(name).or_default();
        let idx = *n;
        *n += 1;
        let m = Universe::new(name, ub(base, v1), Mutant::None);
        let (hash, full) = table_hash(&m);
        if print {
            println!("{name} #{idx}: {hash}\n  f(all) = {full:?}");
        }
        got.push((name, idx, hash));
    }
    let want: Vec<(&str, usize, String)> = PINS
        .iter()
        .map(|(a, b, c)| (*a, *b, c.to_string()))
        .collect();
    assert_eq!(
        got, want,
        "f changed on a tiny family; see the module doc (RIVER_V2_PRINT_PINS=1)"
    );
}
