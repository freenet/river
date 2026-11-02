//! Golden pins of `f` on the tiny families: a hash of each family's whole
//! table (every subset, and what `f` keeps of it), keyed by record names so
//! it does not depend on record order. The law suites cannot see a rule
//! that changes what `f` keeps while keeping it lawful (a cap removed, an
//! ordering changed); these pins can.
//!
//! They detect change only: a wrong rule that was in `f` when the hash was
//! taken is pinned as it stands, and the hash also moves with the encoding,
//! which changes tie-breaks. What a person has checked is `f` of the whole
//! universe of U2, U5, U6 and U7, and the named subsets below, each traced
//! through the rules ([`WHOLE`] says why for those; the executable model has
//! diverged from `f` since it was written, so it is not the reference). U1,
//! U3 and U4 have named subsets in `mechanisms.rs`, and their [`WHOLE`] rows
//! only record the table.
//!
//! To change a pin for a change to `f` that is meant: run the test with
//! `RIVER_V2_PIN_TABLES=<dir>` before and after (each family's table, one
//! subset per line, by record name), `diff -r` the two directories and check
//! the difference against the change, then update the hash.

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
    ("U7 legacy chain", 0, "0f6311e57408d63d"),
    ("U7 legacy chain", 1, "a9a2818d9297d2ab"),
];

/// `f` of each family's whole universe, by record name, each read against
/// the rules. Same order as [`PINS`].
const WHOLE: &[&[&str]] = &[
    &[
        "A1[k1 I1<-k0 s1 c0]",
        "A3[k3 I3<-k1 s1 c1]",
        "A4[k4 I4<-k3 s1 c2]",
        "ckpt v2(retire<2 keep[A1,A3])",
        "clock(3)",
        "restore(A3@2)",
        "act(k3@2)",
        "act(k4@3)",
        "cfg v1",
    ],
    &[
        "A1[k1 I1<-k0 s1 c0]",
        "A3[k3 I3<-k1 s1 c1]",
        "ckpt v2(retire<2 keep[A1,A3])",
        "clock(3)",
        "restore(A3@2)",
        "act(k3@2)",
        "cfg v1",
    ],
    // One link per member, so A4 goes (k1 holds A1); max_links 3, so A5 at
    // depth 2 goes after A1, A2 and A3. One member slot: activity is capped
    // at the clock (2), k2 and k3 tie above k1 and k2 is first in position,
    // so k1's message goes. Links and activity stay for members without one.
    &[
        "A1[k1 I1<-k0 s1 c0]",
        "A2[k2 I1<-k0 s1 c0]",
        "A3[k3 I2<-k0 s2 c0]",
        "act(k1@1)",
        "act(k2@4)",
        "act(k3@3)",
        "clock(2)",
        "cfg v1",
    ],
    // Every link fits: the owner may have 9 children, k3 has one, A5 is at
    // the depth limit of 2, and a member may hold two links. One member slot,
    // as above, so k1's message goes.
    &[
        "A1[k1 I1<-k0 s1 c0]",
        "A2[k2 I1<-k0 s1 c0]",
        "A3[k3 I2<-k0 s2 c0]",
        "A4[k1 I2<-k0 s2 c0]",
        "A5[k2 I3<-k3 s1 c0]",
        "act(k1@1)",
        "act(k2@4)",
        "act(k3@3)",
        "clock(2)",
        "cfg v1",
    ],
    &[
        "A1[k1 I1<-k0 s1 c0]",
        "A4[k4 I4<-k0 s2 c0]",
        "O2:oban(k2@1r0)",
        "D3:dban(k4->k3@2r0)",
        "D5:dban(k4->k5@3r0)",
        "cfg v1",
    ],
    &[
        "A1[k1 I1<-k0 s1 c0]",
        "A2[k2 I2<-k1 s1 c0]",
        "A4[k4 I4<-k0 s2 c0]",
        "D3:dban(k4->k3@2r0)",
        "D5:dban(k4->k5@3r0)",
        "cfg v1",
    ],
    &[
        "A1[k1 I1<-k0 s1 c0]",
        "A2[k2 I2<-k0 s2 c0]",
        "A3[k3 I3<-k1 s1 c0]",
        "L3:legacy mban(A2->k3@2r0)",
        "roster s1[L3]",
        "act(k1@5)",
        "act(k3@4)",
        "clock(8)",
        "msg(k2@9r8)",
        "cfg v1",
    ],
    // One member slot. There is no Clock, so activity does not rank, and it
    // goes to k1, first in position order. Its message stays;
    // k3's goes, and both DMs, whose recipient k3 has no slot. k1's older
    // activity record goes for its newer one.
    &[
        "A1[k1 I1<-k0 s1 c0]",
        "A2[k2 I2<-k0 s2 c0]",
        "A3[k3 I3<-k0 s3 c0]",
        "act(k1@3)",
        "act(k2@2)",
        "act(k3@1)",
        "msg(k1@1r0)",
        "cfg v1",
    ],
    // Window closed: of the legacy links only the rostered G1 counts, and it
    // holds no quota, so A1 stays with it. The owner's ban removes k3 and k3's
    // activity; G2 and k2's activity go as unrostered.
    &[
        "A1[k1 I1<-k0 s1 c0]",
        "G1[k1 leg<-k0]",
        "O3:oban(k3@1r0)",
        "roster s1[G1]",
        "cfg v1",
    ],
    // Window closed: G3 is rostered, but its chain runs through the
    // unrostered G2, so it has no parent and its batch stays with nothing to
    // show for it. G2, G4 and G6 and the legacy ban and activity are
    // unrostered and go. A5 is an accepted link and is unaffected.
    &[
        "G1[k1 leg<-k0]",
        "A5[k5 I1<-k0 s1 c0]",
        "roster s1[G1]",
        "roster s2[G3]",
        "clock(3)",
        "cfg v1",
    ],
    // Window open, so unrostered legacy links count. One child for the owner
    // beyond rostered ones: G1 is exempt, and G4, legacy, takes the slot
    // before A5, so A5 goes and with it G6 under it. L4 stays but does not
    // remove G4: k4 is not under its issuer. G2 is under k1 (child limit 1).
    &[
        "G1[k1 leg<-k0]",
        "G2[k2 leg<-k1]",
        "G3[k3 leg<-k2]",
        "G4[k4 leg<-k0]",
        "roster s1[G1]",
        "roster s2[G3]",
        "lact(k2@2)",
        "lact(k3@6)",
        "L4:legacy mban(G1->k4@5r0)",
        "clock(3)",
        "cfg v1",
    ],
];

/// The families, U6 once (`legacy_families` repeats it).
fn pinned_families() -> Vec<super::families::Family> {
    tiny_families()
        .into_iter()
        .chain(
            legacy_families()
                .into_iter()
                .filter(|f| !f.0.starts_with("U6")),
        )
        .collect()
}

/// One subset per line, by record name, for reading a change as a diff.
fn write_table(m: &Universe, dir: &str, file: &str) {
    use std::fmt::Write;
    let name_set = |s: S| {
        let mut v: Vec<&str> = bits(s).map(|i| m.names[i].as_str()).collect();
        v.sort();
        v.join(", ")
    };
    let mut lines: Vec<String> = (0..(1u128 << m.n()))
        .map(|s| {
            let mut l = String::new();
            write!(l, "{{{}}} -> {{{}}}", name_set(s), name_set(m.f(s))).unwrap();
            l
        })
        .collect();
    lines.sort();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(format!("{dir}/{file}.txt"), lines.join("\n")).unwrap();
}

#[test]
fn f_on_the_tiny_families_matches_its_pins() {
    let print = std::env::var("RIVER_V2_PRINT_PINS").is_ok();
    let tables = std::env::var("RIVER_V2_PIN_TABLES").ok();
    let mut seen: std::collections::HashMap<&str, usize> = Default::default();
    let mut got = vec![];
    for (row, (name, ub, base, v1)) in pinned_families().into_iter().enumerate() {
        let n = seen.entry(name).or_default();
        let idx = *n;
        *n += 1;
        let m = Universe::new(name, ub(base, v1), Mutant::None);
        let (hash, full) = table_hash(&m);
        if print {
            println!("{name} #{idx}: {hash}\n  f(all) = {full:?}");
        }
        if let Some(dir) = &tables {
            write_table(&m, dir, &format!("{name}-{idx}").replace(' ', "_"));
        }
        assert_eq!(full, WHOLE[row], "f of the whole of {name} #{idx}");
        got.push((name, idx, hash));
    }
    let want: Vec<(&str, usize, String)> = PINS
        .iter()
        .map(|(a, b, c)| (*a, *b, c.to_string()))
        .collect();
    assert_eq!(
        got, want,
        "f changed on a tiny family; see the module doc (RIVER_V2_PIN_TABLES=<dir>)"
    );
}

/// Named subsets, traced through the rules: what the whole-universe rows
/// above do not show.
#[test]
fn f_on_named_subsets_of_the_tiny_families() {
    let family = |name: &str, idx: usize| {
        let (name, ub, base, v1) = pinned_families()
            .into_iter()
            .filter(|f| f.0 == name)
            .nth(idx)
            .unwrap();
        Universe::new(name, ub(base, v1), Mutant::None)
    };
    let names = |m: &Universe, s: S| {
        let mut v: Vec<String> = bits(s).map(|i| m.names[i].clone()).collect();
        v.sort();
        v
    };
    let without = |m: &Universe, gone: &[&str]| {
        (0..m.n())
            .filter(|&i| !gone.iter().any(|g| m.names[i].starts_with(g)))
            .fold(0 as S, |a, i| a | (1 << i))
    };
    let m = family("U7 legacy chain", 1);
    // Without G4 the owner's slot goes to A5, and G6 stays under it.
    let k = names(&m, m.f(without(&m, &["G4"])));
    assert!(k.iter().any(|n| n.starts_with("A5")), "{k:?}");
    assert!(k.iter().any(|n| n.starts_with("G6")), "{k:?}");
    // Without G1's batch G1 counts against the owner's slot, so G4 and A5 go.
    let k = names(&m, m.f(without(&m, &["roster s1"])));
    assert!(k.iter().any(|n| n.starts_with("G1")), "{k:?}");
    assert!(!k.iter().any(|n| n.starts_with("G4")), "{k:?}");
    assert!(!k.iter().any(|n| n.starts_with("A5")), "{k:?}");
    // Without G1 its children have no chain and its ban has no issuer.
    let k = names(&m, m.f(without(&m, &["G1"])));
    for gone in ["G2", "G3", "L4", "lact"] {
        assert!(!k.iter().any(|n| n.starts_with(gone)), "{gone} in {k:?}");
    }
    let m = family("U2 quotas full", 0);
    // Without A1, A4 is k1's one link, and max_links 3 now has room for it.
    let k = names(&m, m.f(without(&m, &["A1"])));
    assert!(k.iter().any(|n| n.starts_with("A4")), "{k:?}");
    let m = family("U5 cap+DM", 0);
    // No Clock, so activity does not rank: without k1's activity k1 still
    // holds the slot, its message stays and the DMs still go.
    let k = names(&m, m.f(without(&m, &["act(k1"])));
    assert!(k.iter().any(|n| n.starts_with("msg(k1")), "{k:?}");
    assert!(!k.iter().any(|n| n.starts_with("dm(")), "{k:?}");
}
