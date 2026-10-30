//! Tiny exhaustive families for the rules the model's families do not reach,
//! each run under limits small enough to bind. Exhaustive pairs and every
//! triple (sampled at CI size, all at model size) find what random universes
//! miss: a greedy rule whose choice changes when the record it dropped is
//! absent next time.

use super::builder::{with, Builder, T};
use super::harness::{exhaustive, report_failures, row, Counts, LawModel, HEADER, S};
use super::universe::Universe;
use crate::room_state_v2::retain::mutant::Mutant;
use crate::room_state_v2::retain::test_cap::Limits;

struct RuleFamily {
    name: &'static str,
    build: fn() -> Builder,
    limits: Limits,
    /// Whether its limits must change `f` somewhere (a family for a rule
    /// with no test limit has none to change).
    binds: bool,
}

/// k1 holds a child exception, so its children k2 and k3 are subtree roots.
/// k2's subtree holds more links than the limit; restore requests attach
/// under k2 (one subtree), at k1 (each its own), and for a rostered legacy
/// link that ranks ahead of the rest.
fn subtrees() -> Builder {
    let cfg = |except| {
        with(|c| {
            c.max_members = 7;
            c.max_children = 9;
            c.max_depth = 8;
            c.except = except;
            c.window_open = true;
        })
    };
    let mut b = Builder::new();
    b.invitation(1, "O", 1, 0);
    b.acc("A1", 1, 1);
    b.invitation(2, "A1", 1, 0);
    b.acc("A2", 2, 2);
    b.invitation(3, "A1", 2, 0);
    b.acc("A3", 3, 3);
    b.invitation(4, "A2", 1, 0);
    b.acc("C4", 4, 4);
    b.invitation(5, "A2", 2, 0);
    b.acc("C5", 5, 5);
    b.leg("G7", 7, 0);
    b.roster(1, &["G7"]);
    b.restore("C5", 3);
    b.restore("C4", 4);
    b.restore("A3", 2);
    b.restore("A2", 5);
    b.restore("G7", 1);
    b.cfg(0, cfg(""));
    b.cfg(1, cfg("A1"));
    b.finish()
}

/// A rostered legacy chain k1-k2-k3-k4, and an unrostered link for k3 from
/// a shallower member, with a ban through it. The rule that a member with a
/// rostered link admits no unrostered one must read the rostered chain back
/// to the owner, not the presence of a rostered link: a broken chain's link
/// is dropped, and a later merge could not tell.
fn forged_link() -> Builder {
    let cfg = with(|c| {
        c.window_open = true;
        c.max_members = 7;
        c.max_member_bans = 3;
        c.max_per_issuer = 3;
        c.ban_window = 100;
    });
    let mut b = Builder::new();
    b.leg("G1", 1, 0);
    b.leg("G2", 2, 1);
    b.leg("G3", 3, 2);
    b.leg("G4", 4, 3);
    b.roster(1, &["G1", "G2", "G3", "G4"]);
    b.invitation(1, "O", 1, 0);
    b.acc("A5", 1, 5);
    b.leg("F3", 3, 5);
    b.leg("F2", 2, 5);
    b.act(5, 1);
    b.clock(2);
    b.mban("M3", "A5", T::Key(3), 1, 0, false);
    // A ban that breaks the rostered chain: G3 then stays in a state's raw
    // records but is not admitted, and must not block F3.
    b.oban("O2", T::Key(2), 1, 0);
    b.cfg(0, cfg);
    b.finish()
}

fn families() -> Vec<RuleFamily> {
    vec![
        RuleFamily {
            name: "subtrees",
            build: subtrees,
            limits: Limits {
                subtree_links: Some(2),
                subtree_restores: Some(1),
                restores: Some(3),
                ..Limits::default()
            },
            binds: true,
        },
        RuleFamily {
            name: "forged link",
            build: forged_link,
            limits: Limits::default(),
            binds: false,
        },
    ]
}

/// Runs every family; each must bind: `f` under its limits differs from `f`
/// at the ceilings on some subset.
fn run_all(triples: Option<u64>) {
    println!("{HEADER}");
    for fam in families() {
        let m = Universe::with_limits(fam.name, (fam.build)(), fam.limits);
        let free = Universe::new("free", (fam.build)(), Mutant::None);
        assert!(
            m.n() <= 20,
            "{}: {} records, too many for a table",
            fam.name,
            m.n()
        );
        let c: Counts = exhaustive(&m, triples);
        println!("{}", row(fam.name, &format!("{} recs", m.n()), &c));
        if c.bad() {
            report_failures(&m, &c);
        }
        assert!(!c.bad(), "a law failed on the family {}", fam.name);
        let bound = (0..(1 as S) << m.n())
            .filter(|&s| m.f(s) != free.f(s))
            .count();
        println!("  subsets where its limits decided f: {bound}");
        assert!(
            !fam.binds || bound > 0,
            "the limits of {} never bound",
            fam.name
        );
    }
}

#[test]
fn the_rule_families_obey_the_laws() {
    run_all(Some(20_000));
}

#[test]
#[ignore = "every triple of every rule family (release build)"]
fn the_rule_families_obey_the_laws_exhaustively() {
    run_all(None);
}
