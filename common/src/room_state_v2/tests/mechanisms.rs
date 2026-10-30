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

/// Members k1 to k4, three DMs among them (their tokens make envelopes of
/// 1 to 3 tokens), and the envelopes `purges` lists: (recipient, version,
/// how many of the DMs' tokens).
fn purge_room(purges: &[(usize, u64, usize)]) -> super::builder::Builder {
    let mut b = super::builder::Builder::new();
    for (i, k) in [(1u8, 1usize), (2, 2), (3, 3), (4, 4)] {
        b.invitation(i, "O", i as u32, 0);
        b.acc(&format!("A{k}"), i, k);
        b.act(k, 1);
    }
    b.dm(1, 2, 1, 0);
    b.dm(2, 3, 2, 0);
    b.dm(3, 4, 3, 0);
    let dms = b.dm_indexes();
    for &(r, v, n) in purges {
        b.purge(r, v, &dms[..n]);
    }
    b.cfg(
        0,
        super::builder::with(|c| {
            c.max_dms = 3;
            c.max_members = 5;
        }),
    );
    b.finish()
}

fn under_budget(
    b: &super::builder::Builder,
    budget: usize,
    reserve: usize,
) -> (Retained, Vec<String>) {
    use crate::room_state_v2::retain::test_cap;
    test_cap::set_purge(Some(budget));
    test_cap::set_reserve(Some(reserve));
    let r = f_without(b, &[]);
    test_cap::set_purge(None);
    test_cap::set_reserve(None);
    r
}

/// The room-wide purge token budget (decision (t)), with 1 token reserved
/// for each of the 5 kept keys and a pool of 1: an envelope within the
/// reserve always fits; one beyond it draws on the pool, and a recipient
/// whose newest envelope does not fit falls back to its next older one.
#[test]
fn purge_envelopes_within_the_reserve_fit_and_the_rest_share_the_pool() {
    // k2: 1 token; k3: 2 tokens (1 from the pool); k4: 3 tokens (2 from
    // the pool, which has 1) over an older 1-token envelope.
    let b = purge_room(&[(2, 1, 1), (3, 1, 2), (4, 2, 3), (4, 1, 1)]);
    let r = under_budget(&b, 6, 1);
    assert!(kept(&r, "purge(k2") && kept(&r, "purge(k3"));
    assert!(kept(&r, "purge(k4 v1") && !kept(&r, "purge(k4 v2"));
    // A budget that fits everything keeps each recipient's newest.
    let r = under_budget(&b, 11, 1);
    assert!(kept(&r, "purge(k4 v2") && !kept(&r, "purge(k4 v1"));
}

/// A recipient whose key sorts first gains nothing over envelopes within
/// the reserve: its large envelope is refused, theirs are kept.
#[test]
fn a_low_key_with_a_large_purge_envelope_does_not_displace_smaller_ones() {
    use super::builder::key;
    // The recipient among k2..k4 whose key sorts lowest holds a 3-token
    // envelope; the others hold 1 token each. Budget 5, 1 reserved for each
    // of the 5 kept keys: no pool.
    let mut rs = [2usize, 3, 4];
    rs.sort_by_key(|&r| key(r));
    let b = purge_room(&[(rs[0], 1, 3), (rs[1], 1, 1), (rs[2], 1, 1)]);
    let r = under_budget(&b, 5, 1);
    assert!(
        !kept(&r, &format!("purge(k{}", rs[0])),
        "the low key's large envelope"
    );
    assert!(kept(&r, &format!("purge(k{}", rs[1])));
    assert!(kept(&r, &format!("purge(k{}", rs[2])));
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

fn count_kept(r: &(Retained, Vec<String>), prefix: &str) -> usize {
    r.1.iter()
        .zip(&r.0.keep)
        .filter(|(n, k)| n.starts_with(prefix) && **k)
        .count()
}

/// The per-deputy and per-issuer ban limits, the secret version cap, the
/// per-pair DM limit: each over by one keeps exactly the limit.
#[test]
fn per_issuer_limits_and_the_version_cap_bind() {
    use super::builder::{with, Builder, T};
    // A deputy's two bans under a per-deputy limit of one: the newer stays.
    let mut b = Builder::new();
    for (inv, k, seq) in [(1u8, 3usize, 1u32), (2, 4, 2), (3, 5, 3)] {
        b.invitation(inv, "O", seq, 0);
        b.acc(&format!("A{k}"), inv, k);
    }
    b.dban("D3", 4, T::Key(3), 2, 0);
    b.dban("D5", 4, T::Key(5), 3, 0);
    b.cfg(
        0,
        with(|c| {
            c.max_members = 5;
            c.deputies = 1 << 4;
            c.max_deputy_bans = 3;
            c.max_per_deputy = 1;
        }),
    );
    let r = f_without(&b.finish(), &[]);
    assert!(kept(&r, "D5") && !kept(&r, "D3"), "per-deputy limit");

    // A member's two bans in its subtree under a per-issuer limit of one.
    let mut b = Builder::new();
    b.invitation(1, "O", 1, 0);
    b.acc("A1", 1, 1);
    for (inv, k) in [(2u8, 2usize), (3, 3)] {
        b.invitation(inv, "A1", inv as u32, 0);
        b.acc(&format!("A{k}"), inv, k);
    }
    b.act(1, 1);
    b.clock(2);
    b.mban("M2", "A1", T::Key(2), 1, 0, false);
    b.mban("M3", "A1", T::Key(3), 2, 0, false);
    b.cfg(
        0,
        with(|c| {
            c.max_members = 5;
            c.max_member_bans = 3;
            c.max_per_issuer = 1;
            c.ban_window = 100;
        }),
    );
    let r = f_without(&b.finish(), &[]);
    assert_eq!(count_kept(&r, "M"), 1, "per-issuer limit");

    // Two secret versions under a cap of one: the newer stays.
    let mut b = Builder::new();
    b.secret_version(1);
    b.secret_version(2);
    b.cfg(
        0,
        with(|c| {
            c.private = true;
            c.max_versions = 1;
        }),
    );
    let r = f_without(&b.finish(), &[]);
    assert!(
        kept(&r, "secret v2") && !kept(&r, "secret v1"),
        "version cap"
    );

    // One more DM between a pair than the per-pair limit.
    let per_pair = crate::room_state_v2::config::ceiling::DMS_PER_PAIR;
    let mut b = Builder::new();
    for (inv, k) in [(1u8, 1usize), (2, 2)] {
        b.invitation(inv, "O", inv as u32, 0);
        b.acc(&format!("A{k}"), inv, k);
    }
    for t in 0..=per_pair as u32 {
        b.dm(1, 2, t, 0);
    }
    b.cfg(0, with(|c| c.max_dms = 300));
    let r = f_without(&b.finish(), &[]);
    assert_eq!(count_kept(&r, "dm("), per_pair, "per-pair limit");
}

/// Rostered legacy links come first at their depth, so their children rank
/// ahead of other links' children for what the budgets leave.
#[test]
fn rostered_links_rank_first_at_their_depth() {
    use crate::room_state_v2::records::Record;
    // An accepted link at depth 1 whose digest sorts before the rostered
    // legacy link's, so only the rostered-first rule puts G1 first.
    let mut tried = 0;
    // The invitation id sets the one-time key, and so the link's digest.
    for ia in 10u8..40 {
        let mut b = super::builder::Builder::new();
        b.leg("G1", 1, 0);
        b.roster(1, &["G1"]);
        b.invitation(ia, "O", 0, 0);
        b.acc("A", ia, 2);
        // One child each, at depth 2; one slot left for them.
        b.invitation(2, "G1", 0, 0);
        b.acc("CG", 2, 3);
        b.invitation(3, "A", 0, 0);
        b.acc("CA", 3, 4);
        b.cfg(
            0,
            super::builder::with(|c| {
                c.window_open = true;
                c.max_members = 7;
                c.max_links = 3;
                c.max_children = 9;
            }),
        );
        let b = b.finish();
        let digest = |name: &str| {
            let i = b.names.iter().position(|n| n.starts_with(name)).unwrap();
            Record::digest(&b.recs[i])
        };
        if digest("A[") > digest("G1") {
            continue;
        }
        tried += 1;
        let r = f_without(&b, &[]);
        assert!(kept(&r, "G1") && kept(&r, "A["));
        assert!(kept(&r, "CG") && !kept(&r, "CA"), "invitation {ia}");
    }
    assert!(tried > 0, "no key put the accepted link first by digest");
}

/// A record's time counts at most 48 hours past the clock it names, so a
/// far-future message does not outrank a later-referenced one.
#[test]
fn a_far_future_time_is_clamped_to_its_clock_reference() {
    let mut b = super::builder::Builder::new();
    for (inv, k) in [(1u8, 1usize), (2, 2)] {
        b.invitation(inv, "O", inv as u32, 0);
        b.acc(&format!("A{k}"), inv, k);
    }
    b.clock(20);
    // k1: hour 1,000 against clock 1 (counts as hour 49); k2: hour 60
    // against clock 20 (counts as 60).
    b.msg(1, 1_000, 1);
    b.msg(2, 60, 20);
    b.cfg(0, super::builder::with(|c| c.max_msgs = 1));
    let r = f_without(&b.finish(), &[]);
    assert!(kept(&r, "msg(k2") && !kept(&r, "msg(k1"));
}

/// In a room with one subtree-shaped root (the Invite Bot, holding a child
/// exception), one member signing restore requests under itself takes only
/// its own subtree's slots, and an honest member's request for its rostered
/// link stays, though its hours are the lowest.
#[test]
fn one_member_cannot_block_restores_under_a_transparent_link() {
    use crate::room_state_v2::retain::test_cap;
    let cfg = super::builder::with(|c| {
        c.window_open = true;
        c.max_members = 7;
        c.max_children = 9;
        c.max_depth = 8;
        c.except = "G1";
    });
    let mut b = super::builder::Builder::new();
    b.leg("G1", 1, 0);
    b.leg("G2", 2, 1);
    b.leg("G3", 3, 1);
    b.roster(1, &["G1", "G2", "G3"]);
    for (inv, k) in [(1u8, 4usize), (2, 5), (3, 6)] {
        b.invitation(inv, "G3", inv as u32, 0);
        b.acc(&format!("C{k}"), inv, k);
    }
    b.restore("C4", 100);
    b.restore("C5", 101);
    b.restore("C6", 102);
    b.restore("G2", 1);
    b.cfg(0, cfg);
    let b = b.finish();
    let all = f_without(&b, &[]);
    assert!(kept(&all, "restore(G2"), "the honest request stays");
    let attacker = ["restore(C4", "restore(C5", "restore(C6"];
    let n = attacker.iter().filter(|a| kept(&all, a)).count();
    assert_eq!(n, 2, "the member's subtree has its two slots");
    // With one slot in all, the rostered request is the one kept.
    test_cap::set_limits(test_cap::Limits {
        restores: Some(1),
        ..Default::default()
    });
    let one = f_without(&b, &[]);
    test_cap::set_limits(test_cap::Limits::default());
    assert!(kept(&one, "restore(G2"));
    assert_eq!(attacker.iter().filter(|a| kept(&one, a)).count(), 0);
}

/// One member cannot take the room's link slots: links other than rostered
/// ones are limited per subtree, and a child of a link holding an exception
/// starts its own subtree.
#[test]
fn one_subtree_cannot_take_more_than_its_limit_of_links() {
    use crate::room_state_v2::retain::test_cap;
    let cfg = |except: &'static str| {
        super::builder::with(|c| {
            c.max_members = 7;
            c.max_children = 9;
            c.max_depth = 8;
            c.max_links = 9;
            c.except = except;
        })
    };
    let build = |except: &'static str| {
        let mut b = super::builder::Builder::new();
        b.invitation(1, "O", 1, 0);
        b.acc("A1", 1, 1);
        b.invitation(2, "A1", 1, 0);
        b.acc("A2", 2, 2);
        b.invitation(3, "A2", 1, 0);
        b.acc("A3", 3, 3);
        b.invitation(4, "A3", 1, 0);
        b.acc("A4", 4, 4);
        b.invitation(5, "A1", 2, 0);
        b.acc("A5", 5, 5);
        b.cfg(0, cfg(except));
        b.finish()
    };
    test_cap::set_limits(test_cap::Limits {
        subtree_links: Some(3),
        ..Default::default()
    });
    // A1 is the root: it, A2 and the next by depth; the rest are refused.
    let plain = f_without(&build(""), &[]);
    // A1 transparent: A2 and A5 are roots, A2's subtree holds A2, A3, A4.
    let except = f_without(&build("A1"), &[]);
    test_cap::set_limits(test_cap::Limits::default());
    let n = |r: &(Retained, Vec<String>)| {
        ["A1", "A2", "A3", "A4", "A5"]
            .iter()
            .filter(|x| kept(r, x))
            .count()
    };
    assert_eq!(n(&plain), 3);
    assert!(kept(&plain, "A1") && kept(&plain, "A2") && kept(&plain, "A5"));
    assert_eq!(
        n(&except),
        5,
        "the exception's children are their own roots"
    );
}
