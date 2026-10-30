//! The families are not vacuous: each mechanism the model's probe
//! (`r36 show`) confirmed changes the real `f`'s output the same way.

use super::builder::owner;
use super::families::{tiny_families, u1, u3, u4};
use crate::room_state_v2::records::Record;
use crate::room_state_v2::retain::{retain, Retained, Verifier};

fn family(name: &str, n: usize) -> (super::builder::Cfg, super::builder::Cfg) {
    let f = tiny_families();
    let fam = f.iter().filter(|x| x.0.contains(name)).nth(n).unwrap();
    (fam.2, fam.3)
}

/// `f` of the universe without the records whose names start with `drop`.
fn f_without(b: &super::builder::Builder, drop: &[&str]) -> (Retained, Vec<String>) {
    let (recs, names): (Vec<Record>, Vec<String>) = b
        .recs
        .iter()
        .zip(&b.names)
        .filter(|(_, n)| !drop.iter().any(|d| n.starts_with(d)))
        .map(|(r, n)| (r.clone(), n.clone()))
        .unzip();
    (retain(&owner(), &recs, &mut Verifier::new()), names)
}

fn kept(r: &(Retained, Vec<String>), name: &str) -> bool {
    r.1.iter()
        .zip(&r.0.keep)
        .any(|(n, k)| n.starts_with(name) && *k)
}

#[test]
fn checkpoints_and_the_clock_decide_links() {
    let (b0, v1) = family("U1", 0);
    let b = u1(b0, v1);
    let all = f_without(&b, &[]);
    assert!(kept(&all, "A1") && kept(&all, "A3") && kept(&all, "A4"));
    assert!(!kept(&all, "G2"), "the checkpoint retires the legacy link");
    let no_v2 = f_without(&b, &["ckpt v2"]);
    assert!(kept(&no_v2, "A1") && !kept(&no_v2, "A3") && !kept(&no_v2, "A4"));
    let no_clock3 = f_without(&b, &["clock(3)"]);
    assert!(kept(&no_clock3, "A3") && !kept(&no_clock3, "A4"));
}

#[test]
fn deputy_bans_cannot_touch_deputies_and_owner_bans_cut_subtrees() {
    let (b0, v1) = family("U3", 0);
    let b = u3(b0, v1);
    let all = f_without(&b, &[]);
    // v1: deputies [k4]; k5 is not a deputy, so the deputy ban on it holds.
    assert!(kept(&all, "D5") && !kept(&all, "A5"));
    // The owner ban on k2 removes A2 and so A3 below it.
    assert!(kept(&all, "O2") && !kept(&all, "A2") && !kept(&all, "A3"));
    // Under the base configuration k5 is a deputy: the ban is invalid.
    let base = f_without(&b, &["cfg v1"]);
    assert!(!kept(&base, "D5") && kept(&base, "A5"));
    // Deputies never ban invitations.
    assert!(!kept(&all, "DI") && kept(&all, "A1"));
}

#[test]
fn the_clock_lapses_an_inactive_issuers_member_ban() {
    let (b0, v1) = family("U4", 0);
    let b = u4(b0, v1);
    let all = f_without(&b, &[]);
    // k1 last active at 5, window 0, Clock 8: its ban on k3 is not valid.
    assert!(!kept(&all, "M3"));
    let no_clock8 = f_without(&b, &["clock(8)"]);
    assert!(kept(&no_clock8, "M3"));
    // The legacy member ban is exempt from the activity condition.
    assert!(kept(&all, "L3") && kept(&no_clock8, "L3"));
}

/// The room-wide purge token budget (decision (t)): a recipient's newest
/// envelope that does not fit the budget left is skipped, and its next older
/// envelope that fits is kept in its place.
#[test]
fn a_purge_envelope_over_the_budget_falls_back_to_the_older_one() {
    use crate::room_state_v2::retain::test_cap;
    let cfg = super::builder::with(|c| c.max_dms = 2);
    let mut b = super::builder::Builder::new();
    for (i, k) in [(1u8, 1usize), (2, 2), (3, 3)] {
        b.invitation(i, "O", i as u32, 0);
        b.acc(&format!("A{k}"), i, k);
    }
    b.act(1, 1);
    b.act(2, 1);
    b.act(3, 1);
    b.dm(1, 3, 1, 0);
    b.dm(2, 3, 2, 0);
    let dms = b.dm_indexes();
    b.purge(3, 2, &dms);
    b.purge(3, 1, &dms[..1]);
    b.cfg(0, cfg);
    let b = b.finish();
    let run = |budget: Option<usize>| {
        test_cap::set_purge(budget);
        let r = f_without(&b, &[]);
        test_cap::set_purge(None);
        r
    };
    // Without a binding budget the newest envelope (both DMs) is kept.
    let free = run(None);
    assert!(kept(&free, "purge(k3 v2") && !kept(&free, "purge(k3 v1"));
    assert!(!kept(&free, "dm(k1,k3") && !kept(&free, "dm(k2,k3"));
    // With one token left, the two-token envelope does not fit: the older
    // one-token envelope is kept, and only its DM is purged.
    let one = run(Some(1));
    assert!(!kept(&one, "purge(k3 v2") && kept(&one, "purge(k3 v1"));
    assert!(!kept(&one, "dm(k1,k3") && kept(&one, "dm(k2,k3"));
    // With none, neither fits and both DMs stay.
    let none = run(Some(0));
    assert!(!kept(&none, "purge(k3"));
    assert!(kept(&none, "dm(k1,k3") && kept(&none, "dm(k2,k3"));
}

/// Decision (o): every link but a rostered legacy one shares `max_links`
/// less the roster's link count, so a member minting invitations cannot
/// push an attested legacy link out through the total cap, at any depth.
#[test]
fn invitations_cannot_push_a_rostered_legacy_link_out() {
    let cfg = super::builder::with(|c| {
        c.window_open = true;
        c.max_members = 7;
        c.max_links = 5;
        c.max_children = 9;
        c.max_depth = 8;
    });
    let mut b = super::builder::Builder::new();
    b.leg("G1", 1, 0);
    b.leg("G2", 2, 1);
    b.leg("G3", 3, 2);
    b.roster(1, &["G1", "G2", "G3"]);
    b.invitation(1, "O", 1, 0);
    b.acc("A4", 1, 4);
    for (inv, k) in [(2u8, 5usize), (3, 6), (4, 7)] {
        b.invitation(inv, "A4", inv as u32, 0);
        b.acc(&format!("C{k}"), inv, k);
    }
    b.cfg(0, cfg);
    let b = b.finish();
    let all = f_without(&b, &[]);
    for g in ["G1", "G2", "G3"] {
        assert!(kept(&all, g), "{g} was pushed out");
    }
    // Two slots are left for the other links: A4 and its first child.
    let others = ["A4", "C5", "C6", "C7"]
        .iter()
        .filter(|n| kept(&all, n))
        .count();
    assert_eq!(others, 2);
    assert!(kept(&all, "A4") && kept(&all, "C5"));
}

/// Decision (c): a rostered legacy chain is kept below the depth limit; an
/// unrostered one stops at it.
#[test]
fn only_rostered_legacy_links_go_below_the_depth_limit() {
    let build = |rostered: bool| {
        let cfg = super::builder::with(|c| {
            c.window_open = true;
            c.max_members = 7;
            c.max_depth = 2;
        });
        let mut b = super::builder::Builder::new();
        let names = ["G1", "G2", "G3", "G4", "G5", "G6", "G7"];
        for (n, name) in names.iter().enumerate() {
            b.leg(name, n + 1, n);
        }
        if rostered {
            b.roster(1, &names);
        }
        b.cfg(0, cfg);
        b.finish()
    };
    let all = f_without(&build(true), &[]);
    for g in ["G1", "G2", "G3", "G4", "G5", "G6", "G7"] {
        assert!(kept(&all, g), "rostered {g} at depth past the limit");
    }
    let open = f_without(&build(false), &[]);
    assert!(kept(&open, "G1") && kept(&open, "G2"));
    for g in ["G3", "G4", "G5", "G6", "G7"] {
        assert!(!kept(&open, g), "unrostered {g} below the depth limit");
    }
}
