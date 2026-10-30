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

/// A rostered chain and shallower, more active members, under a member cap
/// below their number: rostered members take the slots first.
fn member_cap() -> Builder {
    let cfg = |n| {
        with(|c| {
            c.window_open = true;
            c.max_members = n;
        })
    };
    let mut b = Builder::new();
    b.leg("G1", 1, 0);
    b.leg("G2", 2, 1);
    b.leg("G3", 3, 2);
    b.roster(1, &["G1", "G2", "G3"]);
    for (inv, k) in [(1u8, 4usize), (2, 5)] {
        b.invitation(inv, "O", inv as u32, 0);
        b.acc(&format!("A{k}"), inv, k);
        b.act(k, 5);
    }
    b.act(3, 1);
    b.msg(3, 1, 0);
    b.msg(4, 2, 0);
    b.oban("O2", T::Key(2), 1, 0);
    b.cfg(0, cfg(2));
    b.cfg(1, cfg(3));
    b.finish()
}

/// Three members, three DMs, and envelopes of 0 to 3 tokens at two versions
/// (v2 and legacy), so a newer envelope can be refused and an older one kept.
fn purge() -> Builder {
    let mut b = Builder::new();
    for (i, k) in [(1u8, 1usize), (2, 2), (3, 3)] {
        b.invitation(i, "O", i as u32, 0);
        b.acc(&format!("A{k}"), i, k);
    }
    b.dm(1, 2, 1, 0);
    b.dm(2, 3, 2, 0);
    b.dm(3, 1, 3, 0);
    let dms = b.dm_indexes();
    b.purge(1, 2, &dms);
    b.purge(1, 1, &dms[..1]);
    b.purge(2, 1, &dms[..2]);
    b.legacy_purge(3, 2, &dms);
    b.legacy_purge(3, 1, &[]);
    b.cfg(
        0,
        with(|c| {
            c.window_open = true;
            c.max_dms = 3;
            c.max_members = 3;
        }),
    );
    b.finish()
}

/// A rostered chain under a `max_links` that leaves one or two slots for
/// other links, which a member at depth 1 and its children compete for.
fn link_reservation() -> Builder {
    let cfg = |n| {
        with(|c| {
            c.window_open = true;
            c.max_members = 7;
            c.max_children = 9;
            c.max_depth = 8;
            c.max_links = n;
        })
    };
    let mut b = Builder::new();
    b.leg("G1", 1, 0);
    b.leg("G2", 2, 1);
    b.leg("G3", 3, 2);
    b.roster(1, &["G1", "G2", "G3"]);
    b.invitation(1, "O", 1, 0);
    b.acc("A4", 1, 4);
    b.invitation(2, "A4", 1, 0);
    b.acc("C5", 2, 5);
    b.invitation(3, "A4", 2, 0);
    b.acc("C6", 3, 6);
    b.leg("U7", 7, 4);
    b.cfg(0, cfg(4));
    b.cfg(1, cfg(5));
    b.finish()
}

/// Roster batches that repeat entries and overrun the batch and entry limits,
/// with the window closed so what the roster holds decides what counts.
fn roster_budget() -> Builder {
    let cfg = with(|c| {
        c.window_open = false;
        c.max_members = 5;
    });
    let mut b = Builder::new();
    b.leg("G1", 1, 0);
    b.leg("G2", 2, 0);
    b.leg("G3", 3, 0);
    b.leg("G4", 4, 0);
    b.roster(1, &["G1", "G2"]);
    b.roster(2, &["G2", "G3"]);
    b.roster(3, &["G3"]);
    b.roster(4, &["G4"]);
    b.cfg(0, cfg);
    b.finish()
}

fn purge_family(budget: usize, reserve: usize) -> RuleFamily {
    RuleFamily {
        name: Box::leak(format!("purge {budget}/{reserve}").into_boxed_str()),
        build: purge,
        limits: Limits {
            purge: Some(budget),
            reserve: Some(reserve),
            ..Limits::default()
        },
        binds: true,
    }
}

fn families() -> Vec<RuleFamily> {
    vec![
        purge_family(1, 0),
        purge_family(2, 0),
        purge_family(4, 1),
        purge_family(5, 1),
        purge_family(6, 1),
        RuleFamily {
            name: "link reservation",
            build: link_reservation,
            limits: Limits::default(),
            binds: false,
        },
        RuleFamily {
            name: "roster budget",
            build: roster_budget,
            limits: Limits {
                roster_batches: Some(3),
                roster_entries: Some(4),
                ..Limits::default()
            },
            binds: true,
        },
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
            name: "member cap",
            build: member_cap,
            limits: Limits::default(),
            binds: false,
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
