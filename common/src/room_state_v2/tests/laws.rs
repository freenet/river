//! The merge laws over the real `f`: commutativity, idempotence of `f` on
//! raw sets, absorption (`f(f(S ∪ X) ∪ X) == f(S ∪ X)`, the live probe),
//! every merge emitting a valid state, and associativity up to the
//! verifier's heal rule. CI runs sampled triples; the `#[ignore]`d tests
//! run the model's full sizes (`cargo test --release -- --ignored`).

use super::builder::owner;
use super::families::{
    legacy_families, rand_cfg, rand_universe, rand_universe_l5, rand_universe_legacy, tiny_families,
};
use super::harness::{
    bits, exhaustive, random, report_failures, row, Counts, LawModel, Rng, HEADER, S,
};
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

/// Which random universe generator a run uses.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Gen {
    /// The model's ([`rand_universe`]).
    Model,
    /// With the member-dependent and legacy records ([`rand_universe_l5`]).
    L5,
    /// With several legacy links in chains ([`rand_universe_legacy`]).
    Legacy,
}

pub struct RandomRun {
    pub tot: Counts,
    /// Universes with a law failure.
    pub detected: u64,
    pub fams: u64,
    /// The smallest pool of valid states any universe produced.
    pub min_states: u64,
    /// The pool size asked for.
    pub pool: usize,
    pub universes: Vec<Universe>,
}

/// The configurations and universe of random family `fi`. The window-close
/// hour comes from its own generator, so adding it changed no other draw.
pub fn rand_family(seed0: u64, fi: u64, gen: Gen) -> super::builder::Builder {
    rand_family_closed(seed0, fi, gen, None)
}

/// As [`rand_family`], with every configuration's window-close hour set to
/// `closed` when given.
fn rand_family_closed(
    seed0: u64,
    fi: u64,
    gen: Gen,
    closed: Option<u32>,
) -> super::builder::Builder {
    let mut rng = Rng(seed0 * 100_000 + fi);
    let keys = 4 + rng.below(2) as usize;
    let mut close = Rng(seed0 * 100_000 + fi + (1 << 40));
    let mut cfg = |rng: &mut Rng| {
        let mut c = rand_cfg(rng, keys);
        // Legacy times in the generators run from 0 to about 40 hours.
        c.closed_at = closed.unwrap_or(close.below(48) as u32);
        c
    };
    let (c0, v1, v2) = (cfg(&mut rng), cfg(&mut rng), cfg(&mut rng));
    match gen {
        Gen::Model => rand_universe(&mut rng, keys, c0, v1, v2),
        Gen::L5 => rand_universe_l5(&mut rng, keys, c0, v1, v2),
        Gen::Legacy => rand_universe_legacy(&mut rng, keys, c0, v1, v2),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_random(
    mutant: Mutant,
    gen: Gen,
    cap: Option<u32>,
    fams: u64,
    pool: usize,
    pairs: u64,
    triples: u64,
    keep: bool,
) -> RandomRun {
    println!("{HEADER}");
    let seed0 = env("RIVER_V2_SEED", 36);
    let mut tot = Counts::default();
    let mut detected = 0;
    let mut min_states = u64::MAX;
    let mut universes = vec![];
    for fi in 0..fams {
        let b = rand_family(seed0, fi, gen);
        let m = Universe::with_cap(&format!("{mutant:?} / univ{fi}"), b, mutant, cap);
        let c = random(&m, seed0 * 7919 + fi, pool, pairs, triples);
        if c.bad() {
            if detected == 0 {
                println!("{}", row(&m.name, &format!("{} recs", m.recs.len()), &c));
                report_failures(&m, &c);
            }
            detected += 1;
        }
        min_states = min_states.min(c.states);
        tot.states += c.states;
        tot.merge_from(&c);
        if keep {
            universes.push(m);
        }
    }
    println!(
        "{}",
        row(
            &format!("{mutant:?} TOTAL ({detected}/{fams} universes with a failure)"),
            "random",
            &tot
        )
    );
    RandomRun {
        tot,
        detected,
        fams,
        min_states,
        pool,
        universes,
    }
}

/// A run of the design (no mutant) is not vacuous: it covered universes,
/// each yielding at least 10 distinct valid states and all of them together
/// at least half the pools asked for, and enough pairs and triples for some
/// to fail strict associativity and heal. (A universe whose records mostly
/// drop yields a small pool; the floor catches one that yields none.)
fn assert_powered(r: &RandomRun) {
    println!(
        "pools: smallest {}, total {} of {} x {} asked for",
        r.min_states, r.tot.states, r.fams, r.pool
    );
    assert!(r.fams > 0, "no universes ran");
    assert!(
        r.min_states >= 10,
        "a universe yielded only {} valid states",
        r.min_states
    );
    assert!(
        r.tot.states * 2 >= r.fams * r.pool as u64,
        "{} valid states over {} universes",
        r.tot.states,
        r.fams
    );
    assert!(r.tot.pairs >= 100 * r.fams, "{} pairs", r.tot.pairs);
    assert!(r.tot.triples >= 100 * r.fams, "{} triples", r.tot.triples);
    assert!(
        r.tot.strict_fail > 0,
        "no triple failed strict associativity"
    );
    assert!(r.tot.healed > 0, "no triple healed");
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
    assert_tiny_powered(&r, &t);
}

/// A tiny run is not vacuous: every family ran and yielded valid states, and
/// the triples include some that fail strict associativity and heal.
fn assert_tiny_powered(r: &[(String, Counts)], t: &Counts) {
    assert_eq!(r.len(), tiny_families().len(), "a family did not run");
    for (n, c) in r {
        assert!(c.states >= 8, "{n} yields only {} valid states", c.states);
        assert!(c.pairs >= c.states * c.states, "{n}: {} pairs", c.pairs);
    }
    assert!(t.triples >= 1000, "{} triples", t.triples);
    assert!(t.strict_fail > 0, "no triple exercised the heal rule");
    assert!(t.healed > 0, "no triple healed");
}

#[test]
#[ignore = "full size: every triple of every tiny family (minutes, release build)"]
fn tiny_families_obey_the_laws_exhaustively() {
    let r = run_tiny(Mutant::None, None, None);
    let t = total(&r);
    println!("{}", row("design TOTAL", "tiny, every triple", &t));
    assert!(!t.bad());
    assert_tiny_powered(&r, &t);
}

#[test]
fn random_universes_obey_the_laws() {
    let r = run_random(
        Mutant::None,
        Gen::Model,
        None,
        env("RIVER_V2_FAMS", 4),
        env("RIVER_V2_POOL", 400) as usize,
        env("RIVER_V2_PAIRS", 2_000),
        env("RIVER_V2_RTRIPLES", 4_000),
        false,
    );
    assert_eq!(r.detected, 0);
    assert!(!r.tot.bad());
    assert_powered(&r);
}

#[test]
#[ignore = "full size: the model's 40 random universes (release build)"]
fn random_universes_obey_the_laws_at_model_size() {
    let r = run_random(
        Mutant::None,
        Gen::Model,
        None,
        env("RIVER_V2_FAMS", 40),
        env("RIVER_V2_POOL", 5_000) as usize,
        env("RIVER_V2_PAIRS", 200_000),
        env("RIVER_V2_RTRIPLES", 1_000_000),
        false,
    );
    assert_eq!(r.detected, 0);
    assert!(!r.tot.bad());
    assert_powered(&r);
}

// ---- mutants: each makes a lower step delete or hide something an upper
// step read, and the harness must catch it where the model caught it.

/// The counts of the families whose name contains `family`, summed.
fn counts_of(r: &[(String, Counts)], family: &str) -> Counts {
    let v: Vec<(String, Counts)> = r
        .iter()
        .filter(|(n, _)| n.contains(family))
        .cloned()
        .collect();
    assert!(!v.is_empty(), "no family {family} ran");
    total(&v)
}

// Each mutant is caught by the laws it breaks: the assertions name the
// counts that must go non-zero, not only that some count did.

/// Bans read a record (the issuer's message) the message cap can delete:
/// `f` is not idempotent, a merge emits a state `f` would change, and
/// absorption fails.
#[test]
fn mutant_ban_reading_issuer_messages_is_caught() {
    let r = run_tiny(Mutant::PresenceMsgBans, Some(20_000), Some("U4"));
    let c = counts_of(&r, "U4");
    assert!(c.raw_idem_fail > 0, "raw idempotence");
    assert!(c.emitted_invalid > 0, "emitted validity");
    assert!(c.absorb_fail > 0, "absorption");
    assert!(c.bad());
}

/// Members kept only while a message needs them: absorption fails, and
/// associativity fails in a way the heal rule cannot repair (a cycle).
#[test]
fn mutant_upward_retention_is_caught() {
    let r = run_tiny(Mutant::UpwardRetention, Some(20_000), Some("U5"));
    let c = counts_of(&r, "U5");
    assert!(c.absorb_fail > 0, "absorption");
    assert!(c.cycle > 0, "associativity (cycle)");
    assert!(c.bad());
}

/// A link whose parent is absent admitted as a root: absorption fails and
/// associativity cycles.
#[test]
fn mutant_no_self_containment_is_caught() {
    let r = run_tiny(Mutant::NoSelfContained, Some(20_000), Some("U1"));
    let c = counts_of(&r, "U1");
    assert!(c.absorb_fail > 0, "absorption");
    assert!(c.cycle > 0, "associativity (cycle)");
    assert!(c.bad());
}

/// A link sweep after the member cap: `f` is not idempotent, a merge emits
/// a state `f` would change, and absorption fails.
#[test]
fn mutant_link_sweep_after_member_cap_is_caught() {
    let r = run_tiny(Mutant::SweepAfterL4, Some(20_000), Some("U1"));
    let c = counts_of(&r, "U1");
    assert!(c.raw_idem_fail > 0, "raw idempotence");
    assert!(c.emitted_invalid > 0, "emitted validity");
    assert!(c.absorb_fail > 0, "absorption");
    assert!(c.bad());
}

#[test]
fn extended_random_universes_obey_the_laws() {
    let r = run_random(
        Mutant::None,
        Gen::L5,
        None,
        env("RIVER_V2_FAMS", 6),
        env("RIVER_V2_POOL", 400) as usize,
        env("RIVER_V2_PAIRS", 2_000),
        env("RIVER_V2_RTRIPLES", 4_000),
        false,
    );
    assert_eq!(r.detected, 0);
    assert!(!r.tot.bad());
    assert_powered(&r);
}

#[test]
#[ignore = "full size: 40 extended random universes (release build)"]
fn extended_random_universes_obey_the_laws_at_model_size() {
    let r = run_random(
        Mutant::None,
        Gen::L5,
        None,
        env("RIVER_V2_FAMS", 40),
        env("RIVER_V2_POOL", 5_000) as usize,
        env("RIVER_V2_PAIRS", 200_000),
        env("RIVER_V2_RTRIPLES", 1_000_000),
        false,
    );
    assert_eq!(r.detected, 0);
    assert!(!r.tot.bad());
    assert_powered(&r);
}

/// The window-close hour matters in the legacy-heavy random universes (the
/// ones the cap and real-path runs use): with it set to 0
/// (every legacy time capped at the epoch), `f` keeps a different set for
/// some subsets, so closed-window legacy time capping is exercised.
#[test]
fn the_window_close_hour_changes_outcomes() {
    let mut differ = 0;
    for fi in 0..12 {
        let m = Universe::new("drawn", rand_family(36, fi, Gen::Legacy), Mutant::None);
        let z = Universe::new(
            "zero",
            rand_family_closed(36, fi, Gen::Legacy, Some(0)),
            Mutant::None,
        );
        assert_eq!(m.n(), z.n());
        let mut r = Rng(fi + 5);
        for _ in 0..300 {
            let s = r.subset(m.n(), 700);
            // The two universes name their records alike.
            if m.show(m.f(s)) != z.show(z.f(s)) {
                differ += 1;
            }
        }
    }
    println!("subsets where the window-close hour changed f: {differ}");
    assert!(differ > 0);
}

/// The extended universes are not vacuous: across f of random subsets, every
/// record kind they add survives in some state.
#[test]
fn extended_universes_keep_every_kind_they_add() {
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

// ---- the legacy link cap (decision (q)), at caps small enough to bind in
// small universes (`retain::test_cap`, which `Universe` sets on every thread
// that computes `f`).

fn legacy_links(m: &Universe, s: S) -> usize {
    bits(s).filter(|&i| m.recs[i].kind() == 10).count()
}

/// Subsets `s` (of `subsets`) where the cap decided the outcome: `f` under
/// the cap keeps exactly `cap` legacy links and `f` without it keeps more.
fn bound(capped: &Universe, free: &Universe, cap: u32, subsets: impl Iterator<Item = S>) -> u64 {
    subsets
        .filter(|&s| {
            legacy_links(capped, capped.f(s)) == cap as usize
                && legacy_links(free, free.f(s)) > cap as usize
        })
        .count() as u64
}

fn run_capped(cap: u32, triples: u64, fams: u64, pool: usize, pairs: u64, rtriples: u64) {
    println!("{HEADER}");
    let mut t = Counts::default();
    let mut bound_tiny = 0;
    for (name, ub, base, v1) in legacy_families() {
        let m = Universe::with_cap(
            &format!("cap {cap} / {name}"),
            ub(base, v1),
            Mutant::None,
            Some(cap),
        );
        let free = Universe::new(name, ub(base, v1), Mutant::None);
        let c = exhaustive(&m, Some(triples));
        println!("{}", row(&m.name, &format!("{} recs", m.recs.len()), &c));
        if c.bad() {
            report_failures(&m, &c);
        }
        let b = bound(&m, &free, cap, 0..(1u128 << m.n()));
        println!("  subsets where the cap of {cap} bound: {b}");
        bound_tiny += b;
        t.states += c.states;
        t.merge_from(&c);
    }
    assert!(!t.bad(), "a law failed under the legacy link cap {cap}");
    assert!(
        bound_tiny > 0,
        "the cap of {cap} never bound in a tiny family"
    );
    let r = run_random(
        Mutant::None,
        Gen::Legacy,
        Some(cap),
        fams,
        pool,
        pairs,
        rtriples,
        true,
    );
    assert_eq!(r.detected, 0);
    assert!(!r.tot.bad());
    assert_powered(&r);
    let seed0 = env("RIVER_V2_SEED", 36);
    let mut bound_random = 0;
    for (fi, m) in r.universes.iter().enumerate() {
        let free = Universe::new(
            "free",
            rand_family(seed0, fi as u64, Gen::Legacy),
            Mutant::None,
        );
        let mut rng = Rng(fi as u64 + 77);
        let n = m.n();
        bound_random += bound(m, &free, cap, (0..300).map(|_| rng.subset(n, 700)));
    }
    println!("random subsets where the cap of {cap} bound: {bound_random}");
    assert!(
        bound_random > 0,
        "the cap of {cap} never bound in a random universe"
    );
}

#[test]
fn the_laws_hold_where_the_legacy_link_cap_binds() {
    for cap in [1, 2] {
        run_capped(
            cap,
            env("RIVER_V2_TRIPLES", 10_000),
            env("RIVER_V2_FAMS", 4),
            env("RIVER_V2_POOL", 300) as usize,
            env("RIVER_V2_PAIRS", 1_500),
            env("RIVER_V2_RTRIPLES", 3_000),
        );
    }
}

#[test]
#[ignore = "full size: the legacy link cap at 1 and 2 over 40 legacy-heavy universes (release build)"]
fn the_laws_hold_where_the_legacy_link_cap_binds_at_model_size() {
    for cap in [1, 2] {
        run_capped(
            cap,
            env("RIVER_V2_TRIPLES", 2_000_000),
            env("RIVER_V2_FAMS", 40),
            env("RIVER_V2_POOL", 5_000) as usize,
            env("RIVER_V2_PAIRS", 200_000),
            env("RIVER_V2_RTRIPLES", 1_000_000),
        );
    }
}
