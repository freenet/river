//! A universe of real records as a law model: a state is a bit set over the
//! universe and `f` is [`retain_refs`] on the records the bits select.
//! Small universes precompute `f` for every subset; larger ones memoize.

use super::builder::{owner, Builder};
use super::harness::{bits, LawModel, S};
use crate::room_state_v2::encoding::{Digest, Key};
use crate::room_state_v2::records::Record;
use crate::room_state_v2::retain::mutant::{self, Mutant};
use crate::room_state_v2::retain::{retain_refs, signature_ok, Verifier};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CACHE: RefCell<(u64, HashMap<S, S>, Verifier)> =
        RefCell::new((0, HashMap::new(), Verifier::new()));
}

pub struct Universe {
    id: u64,
    pub name: String,
    owner: Key,
    pub recs: Vec<Record>,
    digests: Vec<Digest>,
    pub names: Vec<String>,
    mutant: Mutant,
    table: Option<Vec<u32>>,
}

impl Universe {
    pub fn new(name: &str, b: Builder, mutant: Mutant) -> Universe {
        let o = owner();
        for (r, n) in b.recs.iter().zip(&b.names) {
            assert!(signature_ok(&o, r), "builder signed {n} wrongly");
        }
        let digests: Vec<Digest> = b.recs.iter().map(|r| r.digest()).collect();
        assert!(b.recs.len() <= 128);
        let mut u = Universe {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            name: name.into(),
            owner: o,
            recs: b.recs,
            digests,
            names: b.names,
            mutant,
            table: None,
        };
        if u.recs.len() <= 20 {
            u.table = Some(u.build_table());
        }
        u
    }

    fn compute(&self, s: S, v: &mut Verifier) -> S {
        mutant::set(self.mutant);
        let idx: Vec<usize> = bits(s).collect();
        let refs: Vec<&Record> = idx.iter().map(|&i| &self.recs[i]).collect();
        let ds: Vec<Digest> = idx.iter().map(|&i| self.digests[i]).collect();
        let r = retain_refs(&self.owner, &refs, ds, v);
        mutant::set(Mutant::None);
        idx.iter()
            .zip(r.keep.iter())
            .filter(|(_, k)| **k)
            .fold(0, |acc, (&i, _)| acc | (1u128 << i))
    }

    fn trusted_verifier(&self) -> Verifier {
        let mut v = Verifier::new();
        v.trust(self.digests.iter().copied());
        v
    }

    fn build_table(&self) -> Vec<u32> {
        let total = 1usize << self.recs.len();
        let nt = super::harness::threads();
        let chunk = total.div_ceil(nt);
        let parts: Vec<Vec<u32>> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..nt)
                .map(|t| {
                    sc.spawn(move || {
                        let mut v = self.trusted_verifier();
                        (t * chunk..((t + 1) * chunk).min(total))
                            .map(|s| self.compute(s as S, &mut v) as u32)
                            .collect::<Vec<u32>>()
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        parts.concat()
    }
}

impl LawModel for Universe {
    fn f(&self, u: S) -> S {
        if let Some(t) = &self.table {
            return t[u as usize] as S;
        }
        CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.0 != self.id {
                *c = (self.id, HashMap::new(), self.trusted_verifier());
            }
            if let Some(&r) = c.1.get(&u) {
                return r;
            }
            if c.1.len() > 4_000_000 {
                c.1.clear();
            }
            let r = self.compute(u, &mut c.2);
            c.1.insert(u, r);
            r
        })
    }
    /// `validate_state`'s rule over a set of records: `f(S) == S` (the
    /// encoding checks hold for any subset of a universe).
    fn valid(&self, s: S) -> bool {
        self.f(s) == s
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
