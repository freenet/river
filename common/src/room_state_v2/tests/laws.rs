//! The merge laws over the real `f`: commutativity, idempotence of `f` on
//! raw sets, absorption (`f(f(S ∪ X) ∪ X) == f(S ∪ X)`, the live probe),
//! every merge emitting a valid state, and associativity up to the
//! verifier's heal rule. CI runs sampled triples; the `#[ignore]`d tests
//! run the model's full sizes (`cargo test --release -- --ignored`).

use super::builder::owner;
use super::families::{rand_cfg, rand_universe, rand_universe_l5, tiny_families};
use super::harness::{exhaustive, random, report_failures, row, Counts, Rng, HEADER};
use super::universe::Universe;
use crate::room_state_v2::retain::mutant::Mutant;

fn env(name: &str, d: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(d)
}

/// Runs every tiny family under `mutant`; returns each family's counts.
fn run_tiny(mutant: Mutant, triples: Option<u64>, only: Option<&str>) -> Vec<(String, Counts)> {
    let _ = owner();
    println!("{HEADER}");
    let mut out = vec![];
    for (name, ub, base, v1) in tiny_families() {
        if only.is_some_and(|o| !name.contains(o)) {
            continue;
        }
        let m = Universe::new(&format!("{mutant:?} / {name}"), ub(base, v1), mutant);
        let c = exhaustive(&m, triples);
        println!("{}", row(&m.name, &format!("{} recs", m.recs.len()), &c));
        if c.bad() {
            report_failures(&m, &c);
        }
        out.push((m.name.clone(), c));
    }
    out
}

fn run_random(
    mutant: Mutant,
    extended: bool,
    fams: u64,
    pool: usize,
    pairs: u64,
    triples: u64,
) -> (Counts, u64) {
    println!("{HEADER}");
    let seed0 = env("RIVER_V2_SEED", 36);
    let mut tot = Counts::default();
    let mut detected = 0;
    for fi in 0..fams {
        let mut rng = Rng(seed0 * 100_000 + fi);
        let keys = 4 + rng.below(2) as usize;
        let (c0, v1, v2) = (
            rand_cfg(&mut rng, keys),
            rand_cfg(&mut rng, keys),
            rand_cfg(&mut rng, keys),
        );
        let b = if extended {
            rand_universe_l5(&mut rng, keys, c0, v1, v2)
        } else {
            rand_universe(&mut rng, keys, c0, v1, v2)
        };
        let m = Universe::new(&format!("{mutant:?} / univ{fi}"), b, mutant);
        let c = random(&m, seed0 * 7919 + fi, pool, pairs, triples);
        if c.bad() {
            if detected == 0 {
                println!("{}", row(&m.name, &format!("{} recs", m.recs.len()), &c));
                report_failures(&m, &c);
            }
            detected += 1;
        }
        tot.states += c.states;
        tot.merge_from(&c);
    }
    println!(
        "{}",
        row(
            &format!("{mutant:?} TOTAL ({detected}/{fams} universes with a failure)"),
            "random",
            &tot
        )
    );
    (tot, detected)
}

fn total(v: &[(String, Counts)]) -> Counts {
    let mut t = Counts::default();
    for (_, c) in v {
        t.states += c.states;
        t.merge_from(c);
    }
    t
}

#[test]
fn tiny_families_obey_the_laws() {
    let r = run_tiny(Mutant::None, Some(env("RIVER_V2_TRIPLES", 20_000)), None);
    let t = total(&r);
    println!("{}", row("design TOTAL", "tiny, sampled triples", &t));
    assert!(!t.bad(), "a law failed on a tiny family");
    assert!(
        t.states > 1000,
        "the families yield too few valid states: {}",
        t.states
    );
    assert!(t.strict_fail > 0, "no triple exercised the heal rule");
}

#[test]
#[ignore = "full size: every triple of every tiny family (minutes, release build)"]
fn tiny_families_obey_the_laws_exhaustively() {
    let r = run_tiny(Mutant::None, None, None);
    let t = total(&r);
    println!("{}", row("design TOTAL", "tiny, every triple", &t));
    assert!(!t.bad());
}

#[test]
fn random_universes_obey_the_laws() {
    let (t, detected) = run_random(
        Mutant::None,
        false,
        env("RIVER_V2_FAMS", 4),
        env("RIVER_V2_POOL", 400) as usize,
        env("RIVER_V2_PAIRS", 2_000),
        env("RIVER_V2_RTRIPLES", 4_000),
    );
    assert_eq!(detected, 0);
    assert!(!t.bad());
}

#[test]
#[ignore = "full size: the model's 40 random universes (release build)"]
fn random_universes_obey_the_laws_at_model_size() {
    let (t, detected) = run_random(
        Mutant::None,
        false,
        env("RIVER_V2_FAMS", 40),
        env("RIVER_V2_POOL", 5_000) as usize,
        env("RIVER_V2_PAIRS", 200_000),
        env("RIVER_V2_RTRIPLES", 1_000_000),
    );
    assert_eq!(detected, 0);
    assert!(!t.bad());
}

// ---- mutants: each makes a lower step delete or hide something an upper
// step read, and the harness must catch it where the model caught it.

fn detected_in(r: &[(String, Counts)], family: &str) -> bool {
    r.iter().any(|(n, c)| n.contains(family) && c.bad())
}

#[test]
fn mutant_ban_reading_issuer_messages_is_caught() {
    let r = run_tiny(Mutant::PresenceMsgBans, Some(20_000), Some("U4"));
    assert!(detected_in(&r, "U4"));
}

#[test]
fn mutant_upward_retention_is_caught() {
    let r = run_tiny(Mutant::UpwardRetention, Some(20_000), Some("U5"));
    assert!(detected_in(&r, "U5"));
}

#[test]
fn mutant_no_self_containment_is_caught() {
    let r = run_tiny(Mutant::NoSelfContained, Some(20_000), Some("U1"));
    assert!(detected_in(&r, "U1"));
}

#[test]
fn mutant_link_sweep_after_member_cap_is_caught() {
    let r = run_tiny(Mutant::SweepAfterL4, Some(20_000), None);
    let (_, detected) = if r.iter().any(|(_, c)| c.bad()) {
        (Counts::default(), 1)
    } else {
        run_random(Mutant::SweepAfterL4, false, 12, 300, 1_000, 3_000)
    };
    assert!(detected > 0);
}

#[test]
fn extended_random_universes_obey_the_laws() {
    let (t, detected) = run_random(
        Mutant::None,
        true,
        env("RIVER_V2_FAMS", 6),
        env("RIVER_V2_POOL", 400) as usize,
        env("RIVER_V2_PAIRS", 2_000),
        env("RIVER_V2_RTRIPLES", 4_000),
    );
    assert_eq!(detected, 0);
    assert!(!t.bad());
}

#[test]
#[ignore = "full size: 40 extended random universes (release build)"]
fn extended_random_universes_obey_the_laws_at_model_size() {
    let (t, detected) = run_random(
        Mutant::None,
        true,
        env("RIVER_V2_FAMS", 40),
        env("RIVER_V2_POOL", 5_000) as usize,
        env("RIVER_V2_PAIRS", 200_000),
        env("RIVER_V2_RTRIPLES", 1_000_000),
    );
    assert_eq!(detected, 0);
    assert!(!t.bad());
}

/// The extended universes are not vacuous: across f of random subsets, every
/// record kind they add survives in some state.
#[test]
fn extended_universes_keep_every_kind_they_add() {
    use super::harness::{bits, LawModel};
    let mut seen = [0u32; 24];
    for fi in 0..12 {
        let mut rng = Rng(36 * 100_000 + fi);
        let keys = 4 + rng.below(2) as usize;
        let (c0, v1, v2) = (
            rand_cfg(&mut rng, keys),
            rand_cfg(&mut rng, keys),
            rand_cfg(&mut rng, keys),
        );
        let m = Universe::new(
            "cov",
            rand_universe_l5(&mut rng, keys, c0, v1, v2),
            Mutant::None,
        );
        let mut r = Rng(fi);
        for _ in 0..300 {
            let s = m.f(r.subset(m.n(), 700));
            for i in bits(s) {
                seen[m.recs[i].kind() as usize] += 1;
            }
        }
    }
    println!("kept per kind: {seen:?}");
    // Every kind but the legacy configuration and the upgrade pointer.
    for k in [
        2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
    ] {
        assert!(seen[k] > 0, "kind {k} never survives");
    }
}
