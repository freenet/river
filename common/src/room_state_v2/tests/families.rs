//! The executable model's universes, ported to real records: the nine
//! exhaustive tiny families and the random universe generator. Differences
//! from the model, all forced by the design's r4 rules or by real keys:
//! no revocations and no per-invitation use limit (both removed in r4); a
//! v1 64-bit id matches one real key, so an id-set target names its lowest
//! key; a member ban targets a key (the design gives member bans no
//! invitation target); the model's base configuration is a version-0
//! configuration record, so subsets without any configuration take the
//! real defaults; a restore request embeds its retired link.

use super::builder::{base, with, Builder, Cfg, T};
use super::harness::Rng;

pub fn u1(base: Cfg, v1: Cfg) -> Builder {
    let mut b = Builder::new();
    b.invitation(1, "O", 1, 0);
    b.acc("A1", 1, 1);
    b.invitation(3, "A1", 1, 1);
    b.acc("A3", 3, 3);
    b.invitation(4, "A3", 1, 2);
    b.acc("A4", 4, 4);
    b.leg("G2", 2, 0);
    b.checkpoint(1, 2, &["A1"]);
    b.checkpoint(2, 2, &["A1", "A3"]);
    b.clock(1);
    b.clock(3);
    b.restore("A3", 2);
    b.restore("A3", 1);
    b.act(3, 2);
    b.act(4, 3);
    b.cfg(0, base);
    b.cfg(1, v1);
    b.finish()
}

pub fn u2(base: Cfg, v1: Cfg) -> Builder {
    let mut b = Builder::new();
    b.invitation(1, "O", 1, 0);
    b.acc("A1", 1, 1);
    b.acc("A2", 1, 2);
    b.invitation(2, "O", 2, 0);
    b.acc("A3", 2, 3);
    b.acc("A4", 2, 1);
    b.invitation(3, "A3", 1, 0);
    b.acc("A5", 3, 2);
    b.act(1, 1);
    b.act(2, 4);
    b.act(3, 3);
    b.clock(2);
    b.msg(1, 1, 0);
    b.cfg(0, base);
    b.cfg(1, v1);
    b.finish()
}

pub fn u3(base: Cfg, v1: Cfg) -> Builder {
    let mut b = Builder::new();
    b.invitation(1, "O", 1, 0);
    b.acc("A1", 1, 1);
    b.invitation(2, "A1", 1, 0);
    b.acc("A2", 2, 2);
    b.invitation(3, "A2", 1, 0);
    b.acc("A3", 3, 3);
    b.invitation(4, "O", 2, 0);
    b.acc("A4", 4, 4);
    b.invitation(5, "O", 3, 0);
    b.acc("A5", 5, 5);
    b.oban("O2", T::Key(2), 1, 0);
    b.dban("D3", 4, T::Key(3), 2, 0);
    b.dban("D5", 4, T::Key(5), 3, 0);
    b.dban("DI", 4, T::Inv(1), 4, 0);
    b.cfg(0, base);
    b.cfg(1, v1);
    b.finish()
}

pub fn u4(base: Cfg, v1: Cfg) -> Builder {
    let mut b = Builder::new();
    b.invitation(1, "O", 1, 0);
    b.acc("A1", 1, 1);
    b.invitation(2, "O", 2, 0);
    b.acc("A2", 2, 2);
    b.invitation(3, "A1", 1, 0);
    b.acc("A3", 3, 3);
    b.mban("M3", "A1", T::Key(3), 1, 0, false);
    b.mban("L3", "A2", T::Key(3), 2, 0, true);
    // The window is closed in both configurations, so the legacy ban is
    // attested (the model has no window rule for it).
    b.roster(1, &["L3"]);
    b.act(1, 1);
    b.act(1, 5);
    b.act(3, 4);
    b.clock(2);
    b.clock(8);
    b.msg(3, 90, 2);
    b.msg(2, 9, 8);
    b.cfg(0, base);
    b.cfg(1, v1);
    b.finish()
}

pub fn u5(base: Cfg, v1: Cfg) -> Builder {
    let mut b = Builder::new();
    for (i, k) in [(1u8, 1usize), (2, 2), (3, 3)] {
        b.invitation(i, "O", i as u32, 0);
        b.acc(&format!("A{k}"), i, k);
    }
    b.act(1, 1);
    b.act(1, 3);
    b.act(2, 2);
    b.act(3, 1);
    b.msg(1, 1, 0);
    b.msg(3, 2, 0);
    b.dm(1, 3, 1, 0);
    b.dm(2, 3, 2, 0);
    b.cfg(0, base);
    b.cfg(1, v1);
    b.finish()
}

pub fn u6(base: Cfg, v1: Cfg) -> Builder {
    let mut b = Builder::new();
    b.invitation(1, "O", 1, 0);
    b.acc("A1", 1, 1);
    b.leg("G1", 1, 0);
    b.leg("G2", 2, 1);
    b.invitation(3, "O", 2, 0);
    b.acc("A3", 3, 3);
    b.act(2, 1);
    b.act(3, 3);
    b.oban("O3", T::Key(3), 1, 0);
    b.roster(1, &["G1"]);
    b.cfg(0, base);
    b.cfg(1, v1);
    b.finish()
}

/// Not in the model: a legacy-heavy room for the legacy link cap. A v1
/// chain three deep (G1 <- owner, G2 <- k1, G3 <- k2), a second root (G4), a
/// legacy link under an accepted link (G6 <- k5), two roster batches (G1;
/// G3, whose chain runs through the unrostered G2), legacy activity for the
/// chain's members and a legacy member ban by k1.
pub fn u7(base: Cfg, v1: Cfg) -> Builder {
    let mut b = Builder::new();
    b.leg("G1", 1, 0);
    b.leg("G2", 2, 1);
    b.leg("G3", 3, 2);
    b.leg("G4", 4, 0);
    b.invitation(1, "O", 1, 0);
    b.acc("A5", 1, 5);
    b.leg("G6", 6, 5);
    b.roster(1, &["G1"]);
    b.roster(2, &["G3"]);
    b.legacy_msg(2, 2, true);
    b.legacy_msg(3, 6, true);
    b.mban("L4", "G1", T::Key(4), 5, 0, true);
    b.clock(3);
    b.cfg(0, base);
    b.cfg(1, v1);
    b.finish()
}

/// The families where the legacy link cap can bind (U6 at cap 1, U7 at
/// caps 1 and 2; U1 has one legacy link, so no cap binds there). U7's
/// configurations: the window open in the base (every legacy link counts)
/// and closed in v1 (only rostered ones), then the reverse with tighter
/// quotas, which rostered links are exempt from.
pub fn legacy_families() -> Vec<Family> {
    let mut v: Vec<Family> = tiny_families()
        .into_iter()
        .filter(|f| f.0.starts_with("U6"))
        .collect();
    v.push((
        "U7 legacy chain",
        u7,
        with(|c| {
            c.window_open = true;
            c.max_members = 5;
        }),
        with(|c| {
            c.window_open = false;
            c.max_members = 4;
            c.closed_at = 4;
        }),
    ));
    v.push((
        "U7 legacy chain",
        u7,
        with(|c| {
            c.window_open = false;
            c.max_members = 3;
            c.closed_at = 1;
        }),
        with(|c| {
            c.window_open = true;
            c.max_members = 5;
            c.owner_children = 1;
            c.max_children = 1;
        }),
    ));
    v
}

pub type Family = (&'static str, fn(Cfg, Cfg) -> Builder, Cfg, Cfg);

/// The model's nine tiny families, with the model's configurations.
pub fn tiny_families() -> Vec<Family> {
    vec![
        (
            "U1 checkpoint",
            u1,
            with(|c| c.max_members = 3),
            with(|c| {
                c.max_members = 1;
                c.window_open = true;
            }),
        ),
        (
            "U1 checkpoint",
            u1,
            with(|c| {
                c.window_open = true;
                c.max_members = 2;
                c.max_depth = 2;
            }),
            with(|c| {
                c.max_members = 3;
                c.max_links = 2;
            }),
        ),
        (
            "U2 quotas full",
            u2,
            base(),
            with(|c| {
                c.max_members = 1;
                c.max_links = 3;
                c.max_links_per_member = 1;
            }),
        ),
        (
            "U2 quotas full",
            u2,
            with(|c| {
                c.max_members = 2;
                c.owner_children = 2;
            }),
            with(|c| {
                c.max_members = 1;
                c.max_children = 1;
                c.max_depth = 2;
            }),
        ),
        (
            "U3 owner+deputy bans",
            u3,
            with(|c| {
                c.deputies = (1 << 4) | (1 << 5);
                c.max_deputy_bans = 2;
                c.max_per_deputy = 2;
            }),
            with(|c| {
                c.deputies = 1 << 4;
                c.protected = 1 << 1;
                c.max_deputy_bans = 2;
                c.max_per_deputy = 2;
                c.max_members = 2;
            }),
        ),
        (
            "U3 owner+deputy bans",
            u3,
            with(|c| {
                c.deputies = 1 << 4;
                c.protected = 1 << 1;
                c.max_deputy_bans = 1;
                c.max_owner_bans = 1;
            }),
            with(|c| {
                c.deputies = 1 << 4;
                c.max_owner_bans = 0;
                c.max_deputy_bans = 3;
                c.max_per_deputy = 3;
            }),
        ),
        (
            "U4 member bans+clock",
            u4,
            with(|c| {
                c.max_members = 3;
                c.max_member_bans = 1;
                c.ban_window = 3;
            }),
            with(|c| {
                c.max_members = 2;
                c.max_member_bans = 2;
                c.ban_window = 0;
                c.max_msgs = 2;
            }),
        ),
        (
            "U5 cap+DM",
            u5,
            with(|c| c.max_members = 2),
            with(|c| {
                c.max_members = 1;
                c.max_msgs = 2;
                c.max_dms = 2;
            }),
        ),
        (
            "U6 legacy",
            u6,
            with(|c| {
                c.window_open = true;
                c.max_members = 3;
            }),
            with(|c| {
                c.window_open = false;
                c.max_members = 1;
                c.max_links_per_member = 1;
            }),
        ),
    ]
}

pub fn rand_cfg(rng: &mut Rng, keys: usize) -> Cfg {
    let r = |rng: &mut Rng, lo: u64, hi: u64| (lo + rng.below(hi - lo + 1)) as u32;
    let mut dep = 0u8;
    let mut prot = 0u8;
    for k in 1..=keys {
        if rng.below(3) == 0 {
            dep |= 1 << k;
        }
        if rng.below(5) == 0 {
            prot |= 1 << k;
        }
    }
    Cfg {
        max_members: r(rng, 1, 5),
        max_msgs: r(rng, 1, 3),
        max_dms: r(rng, 1, 2),
        max_owner_bans: r(rng, 0, 2),
        max_deputy_bans: r(rng, 0, 2),
        max_per_deputy: r(rng, 1, 2),
        max_member_bans: r(rng, 0, 3),
        max_per_issuer: r(rng, 1, 2),
        max_depth: r(rng, 1, 3),
        max_links: r(rng, 2, 9),
        max_links_per_member: r(rng, 1, 2),
        max_children: r(rng, 1, 3),
        owner_children: r(rng, 1, 5),
        deputies: dep,
        protected: prot,
        window_open: rng.below(2) == 0,
        ban_window: r(rng, 0, 4),
        private: false,
        max_versions: 2,
        max_blobs: 2,
        closed_at: 0,
    }
}

/// The model's random universe: 4-5 keys; 3-5 invitations (random parent,
/// seq and invitation clock) with 1-2 acceptances each; 0-2 legacy links;
/// activity; 0-2 clocks; owner, deputy and member bans (some legacy) with
/// clock_refs; maybe a checkpoint pair with restore requests; maybe roster
/// batches; messages and DMs; three configuration versions.
pub fn rand_universe(rng: &mut Rng, keys: usize, c0: Cfg, v1: Cfg, v2: Cfg) -> Builder {
    let mut b = Builder::new();
    let rk = |rng: &mut Rng| 1 + rng.below(keys as u64) as usize;
    let rref = |rng: &mut Rng| {
        if rng.below(2) == 0 {
            0
        } else {
            rng.below(5) as u32
        }
    };
    let mut links: Vec<(String, usize, u32)> = vec![];
    let ninv = 3 + rng.below(3) as u8;
    let mut ln = 0;
    for inv in 1..=ninv {
        let cands: Vec<usize> = (0..links.len()).filter(|&j| links[j].2 < 3).collect();
        let (pname, pd, pm) = if cands.is_empty() || rng.below(3) == 0 {
            ("O".to_string(), 0, 0)
        } else {
            let j = cands[rng.below(cands.len() as u64) as usize];
            (links[j].0.clone(), links[j].2, links[j].1)
        };
        let clk = if rng.below(2) == 0 {
            0
        } else {
            rng.below(5) as u32
        };
        b.invitation(inv, &pname, 1 + rng.below(3) as u32, clk);
        for _ in 0..1 + rng.below(2) {
            let mut k = rk(rng);
            if k == pm {
                k = k % keys + 1;
            }
            ln += 1;
            let name = format!("A{ln}");
            b.acc(&name, inv, k);
            links.push((name, k, pd + 1));
        }
    }
    let mut legs = vec![];
    for _ in 0..rng.below(3) {
        let inviter = if rng.below(3) == 0 { 0 } else { rk(rng) };
        let mut k = rk(rng);
        if k == inviter {
            k = k % keys + 1;
        }
        ln += 1;
        let name = format!("G{ln}");
        b.leg(&name, k, inviter);
        legs.push(name);
    }
    let all: Vec<String> = links
        .iter()
        .map(|l| l.0.clone())
        .chain(legs.iter().cloned())
        .collect();
    for k in 1..=keys {
        for _ in 0..rng.below(3) {
            b.act(k, rng.below(9) as u32);
        }
    }
    for _ in 0..rng.below(3) {
        b.clock(1 + rng.below(6) as u32);
    }
    let rt = |rng: &mut Rng| match rng.below(4) {
        0 => T::Inv(1 + rng.below(ninv as u64) as u8),
        1 => T::Keys((1 << (1 + rng.below(keys as u64))) | (1 << (1 + rng.below(keys as u64)))),
        _ => T::Key(1 + rng.below(keys as u64) as usize),
    };
    let mut t = 1;
    for i in 0..rng.below(3) {
        let (tg, r) = (rt(rng), rref(rng));
        b.oban(&format!("O{i}"), tg, t, r);
        t += 1;
    }
    for i in 0..rng.below(3) {
        let (tg, dk, r) = (rt(rng), rk(rng), rref(rng));
        b.dban(&format!("D{i}"), dk, tg, t, r);
        t += 1;
    }
    for i in 0..1 + rng.below(3) {
        let iss = all[rng.below(all.len() as u64) as usize].clone();
        let tg = T::Key(rk(rng));
        let (r, lg) = (rref(rng), rng.below(4) == 0);
        b.mban(&format!("M{i}"), &iss, tg, t, r, lg);
        t += 1;
    }
    if rng.below(2) == 0 {
        let k1: Vec<String> = all.iter().filter(|_| rng.below(3) == 0).cloned().collect();
        let k2: Vec<String> = all.iter().filter(|_| rng.below(2) == 0).cloned().collect();
        let r1: Vec<&str> = k1.iter().map(|s| s.as_str()).collect();
        let r2: Vec<&str> = k2.iter().map(|s| s.as_str()).collect();
        b.checkpoint(1, 1 + rng.below(4) as u32, &r1);
        b.checkpoint(2, 1 + rng.below(4) as u32, &r2);
        for _ in 0..rng.below(3) {
            let l = links[rng.below(links.len() as u64) as usize].0.clone();
            b.restore(&l, rng.below(6) as u32);
        }
    }
    if !legs.is_empty() && rng.below(2) == 0 {
        let pick: Vec<&str> = legs
            .iter()
            .map(|s| s.as_str())
            .filter(|_| rng.below(2) == 0)
            .collect();
        if !pick.is_empty() {
            b.roster(1, &pick);
        }
        let pick2: Vec<&str> = legs
            .iter()
            .map(|s| s.as_str())
            .filter(|_| rng.below(2) == 0)
            .collect();
        if !pick2.is_empty() {
            b.roster(2, &pick2);
        }
    }
    for i in 0..2 + rng.below(3) {
        let r = rref(rng);
        b.msg(rng.below(keys as u64 + 1) as usize, 1 + i as u32 * 30, r);
    }
    for i in 0..1 + rng.below(2) {
        let a = rk(rng);
        let mut c = rk(rng);
        if c == a {
            c = a % keys + 1;
        }
        let r = rref(rng);
        b.dm(a, c, i as u32 * 40 + 1, r);
    }
    b.cfg(0, c0);
    b.cfg(1, v1);
    b.cfg(2, v2);
    b.finish()
}

/// Not in the model: the model's random universe plus the member-dependent
/// and legacy records it leaves out (member info, secret versions, blobs and
/// private messages, purge envelopes naming DMs, and legacy messages,
/// activity, DMs, member info, purges and blobs, some attested by roster).
pub fn rand_universe_l5(rng: &mut Rng, keys: usize, c0: Cfg, v1: Cfg, v2: Cfg) -> Builder {
    let private = rng.below(2) == 0;
    let set = |mut c: Cfg, rng: &mut Rng| {
        c.private = private;
        c.max_versions = 1 + rng.below(2) as u32;
        c.max_blobs = 1 + rng.below(3) as u32;
        c
    };
    let (c0, v1, v2) = (set(c0, rng), set(v1, rng), set(v2, rng));
    let mut b = rand_universe(rng, keys, c0, v1, v2);
    let rk = |rng: &mut Rng| 1 + rng.below(keys as u64) as usize;
    for _ in 0..rng.below(4) {
        let m = rng.below(keys as u64 + 1) as usize;
        b.member_info(m, 1 + rng.below(2) as u32, b"nick");
    }
    let nver = rng.below(3) as u32;
    for v in 1..=nver {
        b.secret_version(v);
    }
    for i in 0..rng.below(4) {
        b.blob(rk(rng), 1 + rng.below(nver.max(1) as u64) as u32, i as u8);
    }
    for i in 0..rng.below(3) {
        b.private_msg(
            rng.below(keys as u64 + 1) as usize,
            10 + i as u32 * 7,
            1 + rng.below(2) as u32,
        );
    }
    for i in 0..rng.below(3) {
        let a = rk(rng);
        let mut c = rk(rng);
        if c == a {
            c = a % keys + 1;
        }
        b.legacy_dm(a, c, 5 + i as u32 * 11);
    }
    let dms = b.dm_indexes();
    for _ in 0..1 + rng.below(3) {
        let pick: Vec<usize> = dms.iter().copied().filter(|_| rng.below(2) == 0).collect();
        let r = rk(rng);
        if rng.below(2) == 0 {
            b.legacy_purge(r, 1 + rng.below(2), &pick);
        } else {
            b.purge(r, 1 + rng.below(2), &pick);
        }
    }
    for i in 0..rng.below(3) {
        b.legacy_msg(
            rng.below(keys as u64 + 1) as usize,
            3 + i as u32 * 13,
            false,
        );
    }
    for _ in 0..rng.below(3) {
        b.legacy_msg(rk(rng), rng.below(9) as u32, true);
    }
    for _ in 0..rng.below(2) {
        b.legacy_info(rk(rng), 1 + rng.below(2) as u32);
    }
    // The owner's own v1 member info (deputy lists in it confer nothing in
    // v2; it is the owner's nickname).
    for v in 1..=rng.below(3) as u32 {
        let d: Vec<usize> = (1..=keys).filter(|_| rng.below(3) == 0).collect();
        b.legacy_owner_info(v, &d);
    }
    for _ in 0..rng.below(2) {
        b.legacy_blob(rk(rng), 1 + rng.below(nver.max(1) as u64) as u32);
    }
    let legacy: Vec<usize> = (0..b.recs.len())
        .filter(|&i| b.recs[i].legacy_kind().is_some() && rng.below(2) == 0)
        .collect();
    if !legacy.is_empty() && rng.below(2) == 0 {
        b.roster_indexes(7, &legacy);
    }
    b
}

/// Not in the model: [`rand_universe_l5`] plus 2-4 more legacy links, each
/// invited by the owner or by the member of an earlier legacy link (so v1
/// chains several deep), with legacy activity for some of their members, a
/// v1 message by each and some v1 owner bans, some of all of these
/// rostered. For the legacy link cap and closed-window time capping.
pub fn rand_universe_legacy(rng: &mut Rng, keys: usize, c0: Cfg, v1: Cfg, v2: Cfg) -> Builder {
    let mut b = rand_universe_l5(rng, keys, c0, v1, v2);
    let mut members = vec![0usize];
    let mut idx = vec![];
    for n in 0..2 + rng.below(3) as usize {
        let inviter = members[rng.below(members.len() as u64) as usize];
        let mut k = 1 + rng.below(keys as u64) as usize;
        if k == inviter {
            k = k % keys + 1;
        }
        b.leg(&format!("H{n}"), k, inviter);
        idx.push(b.recs.len() - 1);
        members.push(k);
        if rng.below(2) == 0 {
            b.legacy_msg(k, rng.below(9) as u32, true);
        }
        // A v1 message by the new member and a v1 owner ban, at times on
        // either side of the window-close hour (0 to 47), so closed-window
        // capping changes their order against v2 records.
        b.legacy_msg(k, rng.below(48) as u32, false);
        idx.push(b.recs.len() - 1);
        if rng.below(2) == 0 {
            let t = super::builder::T::Keys(1 << (1 + rng.below(keys as u64)));
            b.oban(&format!("OH{n}"), t, rng.below(48) as u32, 0);
            idx.push(b.recs.len() - 1);
        }
    }
    let pick: Vec<usize> = idx.iter().copied().filter(|_| rng.below(2) == 0).collect();
    if !pick.is_empty() {
        b.roster_indexes(8, &pick);
    }
    b
}
