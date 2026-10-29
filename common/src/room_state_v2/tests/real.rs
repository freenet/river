//! The laws over the real contract path, with real signatures. A state is a
//! `RoomStateV2`; `f` is `state::normalize` (the records sorted by digest),
//! `merge` is `state::merge`, validity is `state::validate_state` on the
//! encoded bytes, and the delta path is `apply_delta(A, delta(B,
//! summary(A)))`; each call gets a fresh `Verifier`, so no signature is
//! trusted in advance. The universe holds the records of a family or random
//! universe and forged copies of a sample of them (`forge.rs`), so forged
//! records reach every layer.
//!
//! Every `f` this model computes is also checked against the bit-set
//! [`Universe`] (record order, every signature trusted): `f(S)` must keep
//! exactly what the universe's `f` keeps of `S`'s genuine records. That is
//! the forged-record property (a forged record changes nothing and is never
//! kept) and the agreement of the two orders (digest order and universe
//! order) in one check.

use super::builder::{owner, Builder};
use super::families::{legacy_families, tiny_families};
use super::forge::forgeries;
use super::harness::{bit, bits, random, report_failures, row, Counts, LawModel, Rng, HEADER, S};
use super::laws::{rand_family, Gen};
use super::universe::Universe;
use crate::room_state_v2::encoding::{Digest, Key};
use crate::room_state_v2::records::Record;
use crate::room_state_v2::retain::{retain, test_cap, Verifier};
use crate::room_state_v2::state::{
    apply_delta, merge, normalize, validate_state, Invalid, RoomStateV2,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static NEXT_ID: AtomicU64 = AtomicU64::new(1 << 32);

#[derive(Default)]
struct Memo {
    id: u64,
    f: HashMap<S, S>,
    valid: HashMap<S, bool>,
    merge: HashMap<(S, S), S>,
}

thread_local! {
    static MEMO: RefCell<Memo> = RefCell::new(Memo::default());
}

pub struct Real {
    id: u64,
    pub name: String,
    owner: Key,
    pub recs: Vec<Record>,
    pub names: Vec<String>,
    index: HashMap<Digest, usize>,
    /// Bits of the genuine records (the universe's, at the same indexes).
    pub genuine: S,
    /// Bits of the forged records.
    pub forged: S,
    /// The genuine record each forged record copies, by index.
    pub origin: HashMap<usize, usize>,
    pub u: Universe,
    cap: Option<u32>,
    /// `f` calls checked against the universe, and those that disagreed.
    pub f_checked: AtomicU64,
    pub f_differs: AtomicU64,
    /// `f` outputs holding a forged record.
    pub forged_kept: AtomicU64,
    pub first_differs: Mutex<Option<S>>,
    /// Kinds (bit per kind) of forged records whose signature `f` checked
    /// and refused: the forgeries reached the signature check, not only a
    /// structural filter.
    pub refused_kinds: AtomicU64,
}

impl Real {
    /// The universe of `b` plus, for up to `per_kind` records of each kind
    /// (chosen by `rng`), every forgery of the record, as long as the whole
    /// fits in 128 records.
    pub fn new(
        name: &str,
        mut b: Builder,
        cap: Option<u32>,
        per_kind: usize,
        rng: &mut Rng,
    ) -> Real {
        let o = owner();
        // A generator can build one record twice (equal fields sign alike);
        // a state holds it once, so the universe does too.
        let mut seen = std::collections::HashSet::new();
        let (recs, names): (Vec<Record>, Vec<String>) = std::mem::take(&mut b.recs)
            .into_iter()
            .zip(std::mem::take(&mut b.names))
            .filter(|(r, _)| seen.insert(r.digest()))
            .unzip();
        b.recs = recs;
        b.names = names;
        let u = Universe::lazy(name, b, cap);
        let mut recs = u.recs.clone();
        let mut names = u.names.clone();
        let n = recs.len();
        let mut by_kind: Vec<Vec<usize>> = vec![vec![]; 24];
        for (i, r) in u.recs.iter().enumerate() {
            by_kind[r.kind() as usize].push(i);
        }
        let mut chosen = vec![];
        for mut v in by_kind {
            for _ in 0..per_kind.min(v.len()) {
                chosen.push(v.swap_remove(rng.below(v.len() as u64) as usize));
            }
        }
        chosen.sort();
        let mut origin = HashMap::new();
        for i in chosen {
            for (what, f) in forgeries(&o, &u.recs[i]) {
                if recs.len() < 128 {
                    origin.insert(recs.len(), i);
                    names.push(format!("FORGED {what} of {}", u.names[i]));
                    recs.push(f);
                }
            }
        }
        let index: HashMap<Digest, usize> = recs
            .iter()
            .enumerate()
            .map(|(i, r)| (r.digest(), i))
            .collect();
        assert_eq!(index.len(), recs.len(), "a forgery repeats a record");
        let all = if recs.len() == 128 {
            u128::MAX
        } else {
            (1u128 << recs.len()) - 1
        };
        let genuine = (1u128 << n) - 1;
        Real {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            name: name.into(),
            owner: o,
            recs,
            names,
            index,
            genuine,
            forged: all & !genuine,
            origin,
            u,
            cap,
            f_checked: AtomicU64::new(0),
            f_differs: AtomicU64::new(0),
            forged_kept: AtomicU64::new(0),
            first_differs: Mutex::new(None),
            refused_kinds: AtomicU64::new(0),
        }
    }

    pub fn state(&self, s: S) -> RoomStateV2 {
        RoomStateV2::from_records(bits(s).map(|i| self.recs[i].clone()).collect())
    }

    pub fn bits_of(&self, st: &RoomStateV2) -> S {
        st.records
            .iter()
            .fold(0, |acc, r| acc | bit(self.index[&r.digest()]))
    }

    /// Runs `run` under this model's legacy link cap.
    fn capped<T>(&self, run: impl FnOnce() -> T) -> T {
        test_cap::set(self.cap);
        let out = run();
        test_cap::set(None);
        out
    }

    fn memo<T>(&self, run: impl FnOnce(&mut Memo) -> T) -> T {
        MEMO.with(|m| {
            let mut m = m.borrow_mut();
            if m.id != self.id || m.f.len() + m.merge.len() > 2_000_000 {
                *m = Memo {
                    id: self.id,
                    ..Memo::default()
                };
            }
            run(&mut m)
        })
    }

    fn compute_f(&self, u: S) -> S {
        let recs: Vec<Record> = bits(u).map(|i| self.recs[i].clone()).collect();
        let mut v = Verifier::new();
        let (st, _) = self.capped(|| normalize(&self.owner, recs, &mut v));
        if let Some(d) = v.failed {
            let k = self.recs[self.index[&d]].kind();
            self.refused_kinds.fetch_or(1 << k, Ordering::Relaxed);
        }
        let out = self.bits_of(&st);
        self.f_checked.fetch_add(1, Ordering::Relaxed);
        if out & self.forged != 0 {
            self.forged_kept.fetch_add(1, Ordering::Relaxed);
        }
        if out != self.u.f(u & self.genuine) {
            self.f_differs.fetch_add(1, Ordering::Relaxed);
            self.first_differs.lock().unwrap().get_or_insert(u);
        }
        out
    }

    /// Whether every check of the real path agreed.
    pub fn clean(&self) -> bool {
        self.f_differs.load(Ordering::Relaxed) == 0 && self.forged_kept.load(Ordering::Relaxed) == 0
    }

    pub fn report(&self) {
        println!(
            "  {}: f checked against the universe {} times, differs {}, keeps a forged record {}",
            self.name,
            self.f_checked.load(Ordering::Relaxed),
            self.f_differs.load(Ordering::Relaxed),
            self.forged_kept.load(Ordering::Relaxed)
        );
        if let Some(u) = *self.first_differs.lock().unwrap() {
            println!("  FIRST DIFFERENCE on {}", self.show(u));
            println!("    real f     = {}", self.show(self.f(u)));
            println!("    universe f = {}", self.show(self.u.f(u & self.genuine)));
        }
    }
}

impl LawModel for Real {
    fn f(&self, u: S) -> S {
        if let Some(r) = self.memo(|m| m.f.get(&u).copied()) {
            return r;
        }
        let r = self.compute_f(u);
        self.memo(|m| m.f.insert(u, r));
        r
    }
    fn valid(&self, s: S) -> bool {
        if let Some(r) = self.memo(|m| m.valid.get(&s).copied()) {
            return r;
        }
        let bytes = self.state(s).to_bytes();
        let r = self
            .capped(|| validate_state(&self.owner, &bytes, &mut Verifier::new()))
            .is_ok();
        self.memo(|m| m.valid.insert(s, r));
        r
    }
    fn merge(&self, a: S, b: S) -> S {
        if let Some(r) = self.memo(|m| m.merge.get(&(a, b)).copied()) {
            return r;
        }
        let (sa, sb) = (self.state(a), self.state(b));
        let st = self.capped(|| merge(&self.owner, &sa, &sb, &mut Verifier::new()));
        let r = self.bits_of(&st);
        self.memo(|m| m.merge.insert((a, b), r));
        r
    }
    fn delta_ok(&self, a: S, b: S) -> Option<bool> {
        let (sa, sb) = (self.state(a), self.state(b));
        let delta = sb.delta(&sa.summarize()).unwrap_or_default();
        let out = self.capped(|| apply_delta(&self.owner, &sa, delta, &mut Verifier::new()));
        Some(matches!(out, Ok(st) if self.bits_of(&st) == self.merge(a, b)))
    }
    fn n(&self) -> usize {
        self.recs.len()
    }
    fn show(&self, s: S) -> String {
        let v: Vec<&str> = bits(s).map(|i| self.names[i].as_str()).collect();
        format!("{{{}}}", v.join(", "))
    }
    fn name(&self) -> &str {
        &self.name
    }
}

fn env(name: &str, d: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(d)
}

/// The forged-record checks on one model: `f(G ∪ {r}) == f(G)` for the set
/// `G` of every genuine record and each forged record `r`, and the same with
/// `r`'s original taken out of `G`; then, over `n`
/// samples, for a raw
/// genuine subset `S` and a valid state `V` (from the pool), and each forged
/// record `r`,
/// - `f(S ∪ {r}) == f(S)` (through `normalize`, a fresh `Verifier`);
/// - `validate_state(V ∪ {r})` refuses: a signature failure naming `r`,
///   `r` dropped unexamined (`NotAFixpoint(1)`), or more records of `r`'s
///   kind than a valid state holds (`TooManyRecords`, a second clock or
///   configuration);
/// - `apply_delta(V, D ∪ {r})`, `D` the delta from another valid state,
///   either refuses (naming `r`, or `TooManyRecords`) or, when `f` drops `r`
///   unexamined, equals `merge(V, W)`;
/// - `retain` is invariant under a shuffle of its input slice.
///
/// Returns (checks, failures).
fn forged_checks(m: &Real, pool: &[S], n: usize, rng: &mut Rng) -> (u64, u64) {
    let forged: Vec<usize> = bits(m.forged).collect();
    let (mut checks, mut fails) = (0u64, 0u64);
    let fail = |what: &str, detail: String, fails: &mut u64| {
        if *fails < 5 {
            println!("  FORGED-RECORD FAILURE [{}] {what}: {detail}", m.name);
        }
        *fails += 1;
    };
    // Each forged record beside every genuine one, and in place of the
    // record it copies (so no genuine twin is admitted ahead of it): the
    // contexts most likely to carry it past the structural filters and caps
    // to its signature check.
    let all = m.f(m.genuine);
    for &j in &forged {
        checks += 2;
        if m.f(m.genuine | bit(j)) != all {
            fail(
                "f changed",
                format!("all genuine and {}", m.names[j]),
                &mut fails,
            );
        }
        let without = m.genuine & !bit(m.origin[&j]);
        if m.f(without | bit(j)) != m.f(without) {
            fail(
                "f changed",
                format!("{} in place of its original", m.names[j]),
                &mut fails,
            );
        }
    }
    for _ in 0..n {
        let s = rng.subset(m.n(), 500) & m.genuine;
        let fs = m.f(s);
        let v = pool[rng.below(pool.len() as u64) as usize];
        let w = pool[rng.below(pool.len() as u64) as usize];
        let (sv, sw) = (m.state(v), m.state(w));
        let d = sw.delta(&sv.summarize()).unwrap_or_default();
        let vw = m.merge(v, w);
        for &j in &forged {
            let r = &m.recs[j];
            checks += 1;
            if m.f(s | bit(j)) != fs {
                fail("f changed", m.show(s | bit(j)), &mut fails);
            }
            let bytes = m.state(v | bit(j)).to_bytes();
            match m.capped(|| validate_state(&m.owner, &bytes, &mut Verifier::new())) {
                Err(Invalid::BadSignature(x)) if x == r.digest() => {}
                Err(Invalid::NotAFixpoint(1)) | Err(Invalid::TooManyRecords) => {}
                other => fail(
                    "validate_state",
                    format!("{other:?} for {}", m.names[j]),
                    &mut fails,
                ),
            }
            let mut dr = d.clone();
            dr.push(r.clone());
            match m.capped(|| apply_delta(&m.owner, &sv, dr, &mut Verifier::new())) {
                Err(Invalid::BadSignature(x)) if x == r.digest() => {}
                Err(Invalid::TooManyRecords) => {}
                Ok(st) if m.bits_of(&st) == vw => {}
                Ok(st) => fail(
                    "apply_delta kept",
                    format!("{} with {}", m.show(m.bits_of(&st)), m.names[j]),
                    &mut fails,
                ),
                Err(e) => fail(
                    "apply_delta",
                    format!("{e:?} for {}", m.names[j]),
                    &mut fails,
                ),
            }
        }
        // Shuffle invariance, on a raw subset that may hold forged records.
        checks += 1;
        let raw = rng.subset(m.n(), 500);
        let mut recs: Vec<Record> = bits(raw).map(|i| m.recs[i].clone()).collect();
        let kept = |recs: &[Record]| -> Vec<Digest> {
            let r = m.capped(|| retain(&m.owner, recs, &mut Verifier::new()));
            let mut d: Vec<Digest> = recs
                .iter()
                .zip(&r.keep)
                .filter(|(_, k)| **k)
                .map(|(x, _)| x.digest())
                .collect();
            d.sort();
            d
        };
        let before = kept(&recs);
        for i in (1..recs.len()).rev() {
            recs.swap(i, rng.below(i as u64 + 1) as usize);
        }
        if kept(&recs) != before {
            fail("retain depends on input order", m.show(raw), &mut fails);
        }
    }
    (checks, fails)
}

struct Run {
    counts: Counts,
    models: usize,
    forged: u64,
    forged_checks: u64,
    forged_fails: u64,
    min_pool: u64,
    unclean: usize,
    forged_kinds: u64,
    refused_kinds: u64,
}

#[allow(clippy::too_many_arguments)]
fn run_model(
    m: &Real,
    seed: u64,
    pool: usize,
    pairs: u64,
    triples: u64,
    samples: usize,
    out: &mut Run,
) {
    let c = random(m, seed, pool, pairs, triples);
    println!(
        "{}",
        row(
            &m.name,
            &format!("{} recs ({} forged)", m.recs.len(), m.forged.count_ones()),
            &c
        )
    );
    if c.bad() {
        report_failures(m, &c);
    }
    // A pool of valid states (f outputs) for the forged-record checks.
    let mut rng = Rng(seed);
    let mut p = vec![];
    for _ in 0..pool * 2 {
        let density = 150 + rng.below(700);
        let fs = m.f(rng.subset(m.n(), density));
        if m.valid(fs) {
            p.push(fs);
        }
    }
    let (k, f) = forged_checks(m, &p, samples, &mut Rng(seed ^ 0xf0f0));
    m.report();
    out.refused_kinds |= m.refused_kinds.load(Ordering::Relaxed);
    out.forged_kinds |= bits(m.forged).fold(0, |a, i| a | 1 << m.recs[i].kind());
    out.models += 1;
    out.forged += m.forged.count_ones() as u64;
    out.forged_checks += k;
    out.forged_fails += f;
    out.min_pool = out.min_pool.min(c.states);
    out.unclean += usize::from(!m.clean());
    out.counts.states += c.states;
    out.counts.merge_from(&c);
}

/// The real path over the tiny families, the legacy families (under the
/// legacy link cap `cap`, when set) and `fams` random legacy-heavy
/// universes.
fn run_real(
    cap: Option<u32>,
    fams: u64,
    pool: usize,
    pairs: u64,
    triples: u64,
    samples: usize,
) -> Run {
    let _ = owner();
    println!("{HEADER}");
    let mut out = Run {
        counts: Counts::default(),
        models: 0,
        forged: 0,
        forged_checks: 0,
        forged_fails: 0,
        min_pool: u64::MAX,
        unclean: 0,
        forged_kinds: 0,
        refused_kinds: 0,
    };
    let fam: Vec<_> = if cap.is_some() {
        legacy_families()
    } else {
        tiny_families()
            .into_iter()
            .chain(legacy_families())
            .collect()
    };
    for (fi, (name, ub, base, v1)) in fam.into_iter().enumerate() {
        let mut rng = Rng(fi as u64 + 1);
        let m = Real::new(
            &format!("real {cap:?} / {name}"),
            ub(base, v1),
            cap,
            2,
            &mut rng,
        );
        run_model(
            &m,
            1000 + fi as u64,
            pool,
            pairs,
            triples,
            samples,
            &mut out,
        );
    }
    let seed0 = env("RIVER_V2_SEED", 36);
    for fi in 0..fams {
        let b = rand_family(seed0, fi, Gen::Legacy);
        let mut rng = Rng(seed0 + fi);
        let m = Real::new(&format!("real {cap:?} / univ{fi}"), b, cap, 1, &mut rng);
        run_model(&m, seed0 * 31 + fi, pool, pairs, triples, samples, &mut out);
    }
    println!(
        "{}",
        row(
            &format!("real path {cap:?} TOTAL ({} models)", out.models),
            "sampled",
            &out.counts
        )
    );
    println!(
        "forged records {}, forged-record checks {}, failures {}, models with an f difference {}",
        out.forged, out.forged_checks, out.forged_fails, out.unclean
    );
    let kinds = |m: u64| (0..24).filter(|k| m & (1 << k) != 0).collect::<Vec<_>>();
    println!(
        "kinds forged {:?}; kinds whose forgery f refused at its signature check {:?}",
        kinds(out.forged_kinds),
        kinds(out.refused_kinds)
    );
    out
}

fn assert_real(r: &Run, fams: u64) {
    assert!(!r.counts.bad(), "a law failed on the real path");
    assert_eq!(r.unclean, 0, "the real f differs from the universe's");
    assert_eq!(r.forged_fails, 0, "a forged record changed an outcome");
    assert!(r.models as u64 > fams, "too few models ran");
    assert!(
        r.min_pool >= 10,
        "a model yielded only {} states",
        r.min_pool
    );
    assert!(
        r.forged >= 10 * r.models as u64,
        "{} forged records",
        r.forged
    );
    assert!(r.forged_checks > 0);
    assert!(
        r.counts.pairs >= 50 * r.models as u64,
        "{} pairs",
        r.counts.pairs
    );
    assert_eq!(r.counts.delta_checked, r.counts.pairs);
    assert!(
        r.counts.triples >= 50 * r.models as u64,
        "{} triples",
        r.counts.triples
    );
    assert!(
        r.counts.strict_fail > 0,
        "no triple failed strict associativity"
    );
    assert!(r.counts.healed > 0, "no triple healed");
}

#[test]
fn the_real_path_obeys_the_laws_with_forged_records() {
    let fams = env("RIVER_V2_REAL_FAMS", 2);
    let r = run_real(
        None,
        fams,
        env("RIVER_V2_REAL_POOL", 40) as usize,
        env("RIVER_V2_REAL_PAIRS", 64),
        env("RIVER_V2_REAL_TRIPLES", 64),
        env("RIVER_V2_REAL_SAMPLES", 2) as usize,
    );
    assert_real(&r, fams);
}

#[test]
fn the_real_path_obeys_the_laws_under_a_binding_legacy_link_cap() {
    let fams = env("RIVER_V2_REAL_FAMS", 1);
    let r = run_real(
        Some(1),
        fams,
        env("RIVER_V2_REAL_POOL", 40) as usize,
        env("RIVER_V2_REAL_PAIRS", 64),
        env("RIVER_V2_REAL_TRIPLES", 64),
        env("RIVER_V2_REAL_SAMPLES", 2) as usize,
    );
    assert_real(&r, fams);
}

#[test]
#[ignore = "full size: the real path over every family and 20 random universes (release build)"]
fn the_real_path_obeys_the_laws_with_forged_records_at_size() {
    for cap in [None, Some(1), Some(2)] {
        let fams = env("RIVER_V2_REAL_FAMS", 20);
        let r = run_real(
            cap,
            fams,
            env("RIVER_V2_REAL_POOL", 400) as usize,
            env("RIVER_V2_REAL_PAIRS", 5_000),
            env("RIVER_V2_REAL_TRIPLES", 10_000),
            env("RIVER_V2_REAL_SAMPLES", 40) as usize,
        );
        assert_real(&r, fams);
    }
}
