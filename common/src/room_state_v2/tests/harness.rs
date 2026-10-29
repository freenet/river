//! The law-check harness of the executable model, unchanged in substance:
//! the verifier's heal rule for associativity, the counts, and the
//! exhaustive and random drivers. A state is a bit set over a universe of
//! records; `f` is the real retention function (see `universe.rs`).
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

pub type S = u128;

// The model's verdicts, counts and drivers follow.

pub fn bit(i: usize) -> S {
    1u128 << i
}

pub fn bits(mut s: S) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        if s == 0 {
            None
        } else {
            let i = s.trailing_zeros() as usize;
            s &= s - 1;
            Some(i)
        }
    })
}

pub trait LawModel: Sync {
    fn f(&self, u: S) -> S;
    fn valid(&self, s: S) -> bool;
    fn n(&self) -> usize;
    fn show(&self, s: S) -> String;
    fn name(&self) -> &str;
    fn merge(&self, a: S, b: S) -> S {
        self.f(a | b)
    }
    /// For a model with a delta path: whether `apply_delta(A, delta(B,
    /// summary(A)))` equals `merge(A, B)`. `None`: no delta path.
    fn delta_ok(&self, _a: S, _b: S) -> Option<bool> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verdict {
    StrictHolds,
    Inconclusive,
    InvalidGrouping,
    Healed,
    NotConverged,
    Cycle,
    Redelivery,
    OutOfPasses,
}

pub const MAX_HEAL_PASSES: usize = 4;

pub fn heal<M: LawModel>(m: &M, l: S, r: S, inputs: [S; 3]) -> Verdict {
    let mut conv = m.merge(l, r);
    if m.merge(r, l) != conv {
        return Verdict::NotConverged;
    }
    let absorbed = [l, r, inputs[0], inputs[1], inputs[2]];
    let mut seen = HashSet::new();
    seen.insert(conv);
    for _ in 0..MAX_HEAL_PASSES {
        let mut changed = false;
        for &o in &absorbed {
            let next = m.merge(conv, o);
            if next != conv {
                if m.merge(next, o) != next {
                    return Verdict::Redelivery;
                }
                if !seen.insert(next) {
                    return Verdict::Cycle;
                }
                conv = next;
                changed = true;
            }
        }
        if !changed {
            return Verdict::Healed;
        }
    }
    Verdict::OutOfPasses
}

pub fn assoc<M: LawModel>(m: &M, a: S, b: S, c: S) -> Verdict {
    let ab = m.merge(a, b);
    let bc = m.merge(b, c);
    if !m.valid(ab) || !m.valid(bc) {
        return Verdict::Inconclusive;
    }
    let l = m.merge(ab, c);
    let r = m.merge(a, bc);
    if l == r {
        return Verdict::StrictHolds;
    }
    if !m.valid(l) || !m.valid(r) {
        return Verdict::InvalidGrouping;
    }
    heal(m, l, r, [a, b, c])
}

pub fn is_violation(v: Verdict) -> bool {
    matches!(
        v,
        Verdict::InvalidGrouping | Verdict::NotConverged | Verdict::Cycle | Verdict::Redelivery
    )
}

#[derive(Default, Clone)]
pub struct Counts {
    pub states: u64,
    pub raw_idem_checked: u64,
    pub raw_idem_fail: u64,
    pub pairs: u64,
    pub comm_fail: u64,
    pub absorb_fail: u64,
    pub emitted_invalid: u64,
    pub triples: u64,
    pub strict_fail: u64,
    pub inconclusive: u64,
    pub healed: u64,
    pub invalid_grouping: u64,
    pub not_converged: u64,
    pub cycle: u64,
    pub redelivery: u64,
    pub out_of_passes: u64,
    pub delta_checked: u64,
    pub delta_fail: u64,
    pub first_absorb: Option<(S, S)>,
    pub first_assoc: Option<(S, S, S, Verdict)>,
}

impl Counts {
    pub fn add_triple(&mut self, v: Verdict, t: (S, S, S)) {
        self.triples += 1;
        match v {
            Verdict::StrictHolds => {}
            Verdict::Inconclusive => self.inconclusive += 1,
            _ => {
                self.strict_fail += 1;
                match v {
                    Verdict::Healed => self.healed += 1,
                    Verdict::InvalidGrouping => self.invalid_grouping += 1,
                    Verdict::NotConverged => self.not_converged += 1,
                    Verdict::Cycle => self.cycle += 1,
                    Verdict::Redelivery => self.redelivery += 1,
                    Verdict::OutOfPasses => self.out_of_passes += 1,
                    _ => {}
                }
                if is_violation(v) && self.first_assoc.is_none() {
                    self.first_assoc = Some((t.0, t.1, t.2, v));
                }
            }
        }
    }
    pub fn add_pair<M: LawModel>(&mut self, m: &M, a: S, b: S) {
        self.pairs += 1;
        let ab = m.merge(a, b);
        if ab != m.merge(b, a) {
            self.comm_fail += 1;
        }
        if !m.valid(ab) {
            self.emitted_invalid += 1;
        }
        if m.merge(ab, b) != ab || m.merge(ab, a) != ab {
            self.absorb_fail += 1;
            if self.first_absorb.is_none() {
                self.first_absorb = Some((a, b));
            }
        }
        if let Some(ok) = m.delta_ok(a, b) {
            self.delta_checked += 1;
            if !ok {
                self.delta_fail += 1;
            }
        }
    }
    pub fn merge_from(&mut self, o: &Counts) {
        self.raw_idem_checked += o.raw_idem_checked;
        self.raw_idem_fail += o.raw_idem_fail;
        self.pairs += o.pairs;
        self.comm_fail += o.comm_fail;
        self.absorb_fail += o.absorb_fail;
        self.emitted_invalid += o.emitted_invalid;
        self.triples += o.triples;
        self.strict_fail += o.strict_fail;
        self.inconclusive += o.inconclusive;
        self.healed += o.healed;
        self.invalid_grouping += o.invalid_grouping;
        self.not_converged += o.not_converged;
        self.cycle += o.cycle;
        self.redelivery += o.redelivery;
        self.out_of_passes += o.out_of_passes;
        self.delta_checked += o.delta_checked;
        self.delta_fail += o.delta_fail;
        if self.first_absorb.is_none() {
            self.first_absorb = o.first_absorb;
        }
        if self.first_assoc.is_none() {
            self.first_assoc = o.first_assoc;
        }
    }
    pub fn bad(&self) -> bool {
        self.absorb_fail + self.raw_idem_fail + self.emitted_invalid + self.comm_fail > 0
            || self.delta_fail > 0
            || self.invalid_grouping + self.not_converged + self.cycle + self.redelivery > 0
    }
}

pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn subset(&mut self, n: usize, p_permille: u64) -> S {
        let mut s = 0;
        for i in 0..n {
            if self.below(1000) < p_permille {
                s |= bit(i);
            }
        }
        s
    }
}

thread_local! {
    /// A worker count set by a test on its own thread (see [`with_threads`]).
    static THREADS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Runs `run` with the drivers called from this thread using `n` workers.
pub fn with_threads<T>(n: usize, run: impl FnOnce() -> T) -> T {
    THREADS.with(|t| t.set(Some(n)));
    let out = run();
    THREADS.with(|t| t.set(None));
    out
}

pub fn threads() -> usize {
    if let Some(n) = THREADS.with(|t| t.get()) {
        return n;
    }
    std::env::var("RIVER_V2_LAW_THREADS")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(16)
        })
}

/// Every subset (raw idempotence), every pair of valid states, and either
/// every triple or, when `triple_sample` is set, that many random triples.
pub fn exhaustive<M: LawModel>(m: &M, triple_sample: Option<u64>) -> Counts {
    assert!(m.n() <= 22, "{} records", m.n());
    let total: u64 = 1 << m.n();
    let mut c = Counts::default();
    let nt = threads() as u64;
    let parts: Vec<(Counts, Vec<S>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nt)
            .map(|t| {
                sc.spawn(move || {
                    let mut c = Counts::default();
                    let mut v = vec![];
                    let mut raw = t;
                    while raw < total {
                        let s = raw as S;
                        let fs = m.f(s);
                        c.raw_idem_checked += 1;
                        if m.f(fs) != fs {
                            c.raw_idem_fail += 1;
                        }
                        if m.valid(s) {
                            v.push(s);
                        }
                        raw += nt;
                    }
                    (c, v)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut valid = vec![];
    for (pc, v) in parts {
        c.merge_from(&pc);
        valid.extend(v);
    }
    valid.sort();
    c.states = valid.len() as u64;
    let next = AtomicU64::new(0);
    let parts: Vec<Counts> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nt)
            .map(|_| {
                let valid = &valid;
                let next = &next;
                sc.spawn(move || {
                    let mut c = Counts::default();
                    loop {
                        let ia = next.fetch_add(1, Ordering::Relaxed) as usize;
                        if ia >= valid.len() {
                            break;
                        }
                        let a = valid[ia];
                        for &b in valid {
                            c.add_pair(m, a, b);
                            if triple_sample.is_none() {
                                for &cc in valid {
                                    c.add_triple(assoc(m, a, b, cc), (a, b, cc));
                                }
                            }
                        }
                    }
                    c
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for p in &parts {
        c.merge_from(p);
    }
    if let Some(n) = triple_sample {
        if !valid.is_empty() {
            let valid = &valid;
            let parts = chunked(
                n,
                |k| 0x5eed ^ (k + 1).wrapping_mul(0x9e37_79b9),
                |rng, count| {
                    let mut c = Counts::default();
                    let pick = |rng: &mut Rng| valid[rng.below(valid.len() as u64) as usize];
                    for _ in 0..count {
                        let (a, b, cc) = (pick(rng), pick(rng), pick(rng));
                        c.add_triple(assoc(m, a, b, cc), (a, b, cc));
                    }
                    c
                },
            );
            c.merge_from(&parts);
        }
    }
    c
}

/// The number of chunks sampled work is split into. Fixed, so which pairs
/// and triples are drawn depends only on the seed, not on the thread count.
pub const CHUNKS: u64 = 64;

/// Runs `work` over [`CHUNKS`] chunks that share `total` items, chunk `k`
/// drawing from `Rng(seed(k))`, on [`threads`] workers; merges the chunks'
/// counts in chunk order.
pub fn chunked(
    total: u64,
    seed: impl Fn(u64) -> u64 + Sync,
    work: impl Fn(&mut Rng, u64) -> Counts + Sync,
) -> Counts {
    let next = AtomicU64::new(0);
    let mut parts: Vec<(u64, Counts)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads())
            .map(|_| {
                let (next, seed, work) = (&next, &seed, &work);
                sc.spawn(move || {
                    let mut out = vec![];
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        if k >= CHUNKS {
                            break;
                        }
                        let count = total / CHUNKS + u64::from(k < total % CHUNKS);
                        let mut rng = Rng(seed(k));
                        out.push((k, work(&mut rng, count)));
                    }
                    out
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    parts.sort_by_key(|p| p.0);
    let mut c = Counts::default();
    for (_, p) in &parts {
        c.merge_from(p);
    }
    c
}

pub fn random<M: LawModel>(m: &M, seed: u64, pool_n: usize, pairs: u64, triples: u64) -> Counts {
    let mut rng = Rng(seed);
    let mut c = Counts::default();
    let mut pool_set = HashSet::new();
    let mut pool = vec![];
    let mut tries = 0;
    while pool.len() < pool_n && tries < pool_n * 20 {
        tries += 1;
        let p = 150 + rng.below(700);
        let raw = rng.subset(m.n(), p);
        let fs = m.f(raw);
        c.raw_idem_checked += 1;
        if m.f(fs) != fs {
            c.raw_idem_fail += 1;
            continue;
        }
        if m.valid(fs) && pool_set.insert(fs) {
            pool.push(fs);
        }
    }
    c.states = pool.len() as u64;
    if pool.is_empty() {
        return c;
    }
    let pool = &pool;
    let pick = |rng: &mut Rng| pool[rng.below(pool.len() as u64) as usize];
    // Pairs and triples are split over the chunks separately, so each total
    // is exact.
    let pc = chunked(
        pairs,
        |k| seed ^ (k + 1).wrapping_mul(0x1234_5678_9abc_def1),
        |rng, count| {
            let mut c = Counts::default();
            for _ in 0..count {
                let (a, b) = (pick(rng), pick(rng));
                c.add_pair(m, a, b);
            }
            c
        },
    );
    let tc = chunked(
        triples,
        |k| !seed ^ (k + 1).wrapping_mul(0x1234_5678_9abc_def1),
        |rng, count| {
            let mut c = Counts::default();
            for _ in 0..count {
                let (a, b, cc) = (pick(rng), pick(rng), pick(rng));
                c.add_triple(assoc(m, a, b, cc), (a, b, cc));
            }
            c
        },
    );
    c.merge_from(&pc);
    c.merge_from(&tc);
    c
}

pub fn minimize_triple<M: LawModel>(m: &M, t: (S, S, S, Verdict)) -> (S, S, S, Verdict) {
    let (mut a, mut b, mut c, v) = t;
    loop {
        let mut progress = false;
        for which in 0..3 {
            let cur = [a, b, c][which];
            for r in bits(cur) {
                let cand = m.f(cur & !bit(r));
                if cand == cur || !m.valid(cand) {
                    continue;
                }
                let (na, nb, nc) = match which {
                    0 => (cand, b, c),
                    1 => (a, cand, c),
                    _ => (a, b, cand),
                };
                if assoc(m, na, nb, nc) == v {
                    a = na;
                    b = nb;
                    c = nc;
                    progress = true;
                    break;
                }
            }
        }
        if !progress {
            return (a, b, c, v);
        }
    }
}

pub fn minimize_pair<M: LawModel>(m: &M, (mut a, mut b): (S, S)) -> (S, S) {
    let fails = |a: S, b: S| {
        let ab = m.merge(a, b);
        m.merge(ab, b) != ab || m.merge(ab, a) != ab
    };
    loop {
        let mut progress = false;
        for which in 0..2 {
            let cur = [a, b][which];
            for r in bits(cur) {
                let cand = m.f(cur & !bit(r));
                if cand == cur || !m.valid(cand) {
                    continue;
                }
                let (na, nb) = if which == 0 { (cand, b) } else { (a, cand) };
                if fails(na, nb) {
                    a = na;
                    b = nb;
                    progress = true;
                    break;
                }
            }
        }
        if !progress {
            return (a, b);
        }
    }
}

pub fn row(label: &str, cfgdesc: &str, c: &Counts) -> String {
    format!(
        "| {label} | {cfgdesc} | {} | {}/{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {}/{} |",
        c.states,
        c.raw_idem_fail,
        c.raw_idem_checked,
        c.pairs,
        c.comm_fail,
        c.absorb_fail,
        c.emitted_invalid,
        c.triples,
        c.inconclusive,
        c.strict_fail,
        c.healed,
        c.invalid_grouping,
        c.cycle,
        c.redelivery,
        c.not_converged + c.out_of_passes,
        c.delta_fail,
        c.delta_checked,
    )
}

pub const HEADER: &str = "| model | config | valid states | f not idempotent on raw sets | pairs | comm fail | absorb fail (either dir) | merge emits invalid | triples | inconclusive (invalid intermediate) | strict assoc fail | healed | invalid grouping | cycle | redelivery | not-converged + out-of-passes | delta path differs / checked |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|";

pub fn report_failures<M: LawModel>(m: &M, c: &Counts) {
    if let Some(p) = c.first_absorb {
        let (a, b) = minimize_pair(m, p);
        let ab = m.merge(a, b);
        println!("MIN-ABSORB [{}]", m.name());
        println!("  A = {}", m.show(a));
        println!("  B = {}", m.show(b));
        println!("  merge(A,B) = {}", m.show(ab));
        println!("  merge(merge(A,B),B) = {}", m.show(m.merge(ab, b)));
        println!("  merge(merge(A,B),A) = {}", m.show(m.merge(ab, a)));
    }
    if let Some(t) = c.first_assoc {
        let (a, b, cc, v) = minimize_triple(m, t);
        let l = m.merge(m.merge(a, b), cc);
        let r = m.merge(a, m.merge(b, cc));
        println!("MIN-ASSOC [{}] verdict {:?}", m.name(), v);
        println!("  A = {}", m.show(a));
        println!("  B = {}", m.show(b));
        println!("  C = {}", m.show(cc));
        println!("  (AB)C = {}", m.show(l));
        println!("  A(BC) = {}", m.show(r));
        println!("  merge(L,R) = {}", m.show(m.merge(l, r)));
    }
}
