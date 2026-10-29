//! The law harness tested on toy models whose merge breaks a law in a known
//! way: each must make `assoc` (through `heal`) return the verdict named for
//! it, raise the matching count, and make `Counts::bad` true. Without these,
//! a harness that never reported a violation would look the same as a design
//! that has none.

use super::harness::{assoc, exhaustive, heal, with_threads, LawModel, Verdict, S};

/// States are the values `0..size`; `f` is the identity; every state is
/// valid; `merge` is the given function.
struct Toy {
    name: &'static str,
    bits: usize,
    size: S,
    merge: fn(S, S) -> S,
}

impl LawModel for Toy {
    fn f(&self, u: S) -> S {
        if u < self.size {
            u
        } else {
            0
        }
    }
    fn valid(&self, s: S) -> bool {
        s < self.size
    }
    fn n(&self) -> usize {
        self.bits
    }
    fn show(&self, s: S) -> String {
        s.to_string()
    }
    fn name(&self) -> &str {
        self.name
    }
    fn merge(&self, a: S, b: S) -> S {
        (self.merge)(a, b)
    }
}

/// Rock, paper, scissors: the merge of two states is the one that beats the
/// other (1 beats 0, 2 beats 1, 0 beats 2). Commutative, idempotent and
/// absorbing, but not associative, and merging the two groupings with the
/// inputs goes round the cycle back to a state already seen.
fn pick_one(a: S, b: S) -> S {
    if a == b {
        a
    } else if (a + 1) % 3 == b {
        b
    } else {
        a
    }
}

/// `max(a, b) + 1` (at most 63): commutative, but a state merged again with an input
/// grows each time, so redelivering an input changes the result.
fn max_plus_one(a: S, b: S) -> S {
    (a.max(b) + 1).min(63)
}

/// `2a + b mod 4`: not commutative, so the two groupings do not converge.
fn left_heavy(a: S, b: S) -> S {
    (2 * a + b) % 4
}

#[test]
fn a_cyclic_merge_is_reported_as_a_cycle() {
    let m = Toy {
        name: "pick one",
        bits: 2,
        size: 3,
        merge: pick_one,
    };
    assert_eq!(assoc(&m, 0, 1, 2), Verdict::Cycle);
    let c = exhaustive(&m, None);
    assert_eq!(c.states, 3);
    assert!(c.cycle > 0, "no cycle counted");
    assert_eq!(
        c.comm_fail + c.absorb_fail + c.redelivery + c.not_converged,
        0
    );
    assert!(c.bad());
}

#[test]
fn a_growing_merge_is_reported_as_redelivery() {
    let m = Toy {
        name: "max + 1",
        bits: 6,
        size: 64,
        merge: max_plus_one,
    };
    assert_eq!(assoc(&m, 5, 0, 0), Verdict::Redelivery);
    assert_eq!(heal(&m, 7, 6, [5, 0, 0]), Verdict::Redelivery);
    let c = exhaustive(&m, None);
    assert!(c.redelivery > 0, "no redelivery counted");
    assert!(c.absorb_fail > 0, "no absorption failure counted");
    assert_eq!(c.comm_fail, 0);
    assert!(c.bad());
}

#[test]
fn a_non_commutative_merge_is_reported_as_not_converged() {
    let m = Toy {
        name: "2a + b",
        bits: 2,
        size: 4,
        merge: left_heavy,
    };
    assert_eq!(assoc(&m, 1, 0, 0), Verdict::NotConverged);
    let c = exhaustive(&m, None);
    assert!(c.not_converged > 0, "no non-convergence counted");
    assert!(c.comm_fail > 0, "no commutativity failure counted");
    assert!(c.bad());
}

/// The sampled triples depend on the seed alone: the same run on one worker
/// and on several gives the same counts.
#[test]
fn sampled_triples_do_not_depend_on_the_thread_count() {
    let m = Toy {
        name: "pick one",
        bits: 2,
        size: 3,
        merge: pick_one,
    };
    let run = |t: usize| {
        let c = with_threads(t, || exhaustive(&m, Some(1_001)));
        (
            c.triples,
            c.strict_fail,
            c.cycle,
            c.inconclusive,
            c.first_assoc,
        )
    };
    let one = run(1);
    assert_eq!(one.0, 1_001);
    assert_eq!(one, run(5));
}
