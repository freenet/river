//! Does the proposed fix actually close the commutativity violations, or only
//! the data loss?
//!
//! The proposed fix (freenet/river#671) is: make the HELD DM set obey the same
//! membership rule the incoming path already enforces, before
//! `trim_to_global_cap` runs. This example approximates that by sweeping each
//! input state's DMs with `sweep_after_membership_change` — the same predicate
//! cleanup step 6 uses — BEFORE merging, then re-running the commutativity
//! check over every pair.
//!
//! It is an approximation, not the fix: the real one runs inside `apply_delta`
//! against the member set as of that field, which is not identical to the
//! pre-merge member set. Read a PASS here as "the fix plausibly closes it, go
//! write the real one" and a FAIL as "one change is not enough".
//!
//! Usage: cargo run --release --example merge_law_simfix -- <corpus-dir>

use ciborium::{de::from_reader, ser::into_writer};
use freenet_scaffold::ComposableState;
use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomParametersV1;
use river_core::ChatRoomStateV1;
use std::collections::HashSet;
use std::fs;

fn load<T: serde::de::DeserializeOwned>(p: &str) -> T {
    let b = fs::read(p).unwrap_or_else(|e| panic!("read {p}: {e}"));
    from_reader::<T, &[u8]>(&b[..]).unwrap_or_else(|e| panic!("decode {p}: {e}"))
}

fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = vec![];
    into_writer(s, &mut v).unwrap();
    v
}

fn merge(
    a: &ChatRoomStateV1,
    b: &ChatRoomStateV1,
    p: &ChatRoomParametersV1,
) -> Result<ChatRoomStateV1, String> {
    let mut s = a.clone();
    let parent = s.clone();
    s.merge(&parent, p, b).map_err(|e| e.to_string())?;
    Ok(s)
}

/// Stand-in for the fix: drop held DMs whose endpoints are not live members
/// **in the MERGED context**.
///
/// Sweeping against a state's own pre-merge member set is useless here and was
/// my first mistake: a DM's counterparty is typically still a live member in the
/// state that holds it, and only dies once the other peer's bans arrive. So the
/// context has to be the post-merge one. Bans are add-only and never differ
/// between the two orders (measured), so taking them from either merge is safe;
/// the member set is taken at the cleanup fixpoint, where both orders agree.
fn merged_context(
    a: &ChatRoomStateV1,
    b: &ChatRoomStateV1,
    p: &ChatRoomParametersV1,
) -> (HashSet<MemberId>, HashSet<MemberId>) {
    let mut m = merge(a, b, p).expect("merge for context");
    for _ in 0..8 {
        let before = ser(&m);
        m.post_apply_cleanup(p).unwrap();
        if ser(&m) == before {
            break;
        }
    }
    let banned = m.members.banned_member_ids(&m.bans, &m.member_info, p);
    let active: HashSet<MemberId> = m.members.members.iter().map(|x| x.member.id()).collect();
    (active, banned)
}

fn sweep_with(
    s: &mut ChatRoomStateV1,
    p: &ChatRoomParametersV1,
    active: &HashSet<MemberId>,
    banned: &HashSet<MemberId>,
) {
    s.direct_messages
        .sweep_after_membership_change(p.owner_id(), active, banned);
}

fn diff_fields(x: &ChatRoomStateV1, y: &ChatRoomStateV1) -> Vec<&'static str> {
    let mut d = vec![];
    if x.members != y.members {
        d.push("members");
    }
    if x.member_info != y.member_info {
        d.push("member_info");
    }
    if x.direct_messages != y.direct_messages {
        d.push("direct_messages");
    }
    if x.bans != y.bans {
        d.push("bans");
    }
    if x.recent_messages != y.recent_messages {
        d.push("recent_messages");
    }
    d
}

fn run(tag: &str, states: &[(String, ChatRoomStateV1)], p: &ChatRoomParametersV1, simfix: bool) {
    let mut fails = 0usize;
    let mut pairs = 0usize;
    let mut nonidem = 0usize;
    let mut pruned_total = 0usize;
    let mut tally: std::collections::BTreeMap<String, usize> = Default::default();
    for i in 0..states.len() {
        for j in (i + 1)..states.len() {
            pairs += 1;
            let (mut a, mut b) = (states[i].1.clone(), states[j].1.clone());
            if simfix {
                let (active, banned) = merged_context(&a, &b, p);
                let before = a.direct_messages.messages.len() + b.direct_messages.messages.len();
                sweep_with(&mut a, p, &active, &banned);
                sweep_with(&mut b, p, &active, &banned);
                pruned_total +=
                    before - (a.direct_messages.messages.len() + b.direct_messages.messages.len());
            }
            match (merge(&a, &b, p), merge(&b, &a, p)) {
                (Ok(x), Ok(y)) => {
                    if ser(&x) != ser(&y) {
                        fails += 1;
                        *tally.entry(diff_fields(&x, &y).join("+")).or_default() += 1;
                    }
                    for r in [&x, &y] {
                        let mut again = r.clone();
                        again.post_apply_cleanup(p).unwrap();
                        if ser(&again) != ser(r) {
                            nonidem += 1;
                        }
                    }
                }
                _ => println!("  [{tag}] merge error on pair {i},{j}"),
            }
        }
    }
    println!("[{tag}] commutativity: {fails} failing / {pairs} pairs; non-idempotent results: {nonidem} / {}", pairs * 2);
    if simfix {
        println!("[{tag}] doomed DMs pruned across all pairs: {pruned_total}");
    }
    if !tally.is_empty() {
        println!("[{tag}] differing-field tally: {tally:?}");
    }
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: <corpus-dir>");
    let params: ChatRoomParametersV1 = load(&format!("{dir}/params.bin"));
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let n = e.unwrap().file_name().to_string_lossy().to_string();
            n.starts_with("state_").then_some(n)
        })
        .collect();
    names.sort();
    let base: Vec<(String, ChatRoomStateV1)> = names
        .iter()
        .map(|n| (n.clone(), load(&format!("{dir}/{n}"))))
        .collect();
    println!("loaded {} states", base.len());

    // Announce the plan BEFORE running it, so a section that does not run is
    // visibly missing rather than silently absent.
    //
    // This is not cosmetic. A stale copy of this file (one predating the
    // associativity sweep below) exits cleanly after the commutativity lines,
    // printing no error and no associativity section — which is
    // indistinguishable, to a reader tailing the log, from the sweep still
    // being in progress. That cost a real investigation: two runs were waited
    // on for tens of minutes before anyone noticed the harness could not have
    // produced the number being waited for. Naming the checks up front turns
    // "absent" into "started and never finished", which is a question someone
    // asks rather than one they wait out.
    let plan = [
        "commutativity + idempotence (baseline)",
        "commutativity + idempotence (sim-fix)",
        "associativity (baseline)",
        "associativity (sim-fix)",
    ];
    println!("plan: {} checks", plan.len());
    for (i, what) in plan.iter().enumerate() {
        println!("  [{}/{}] {what}", i + 1, plan.len());
    }
    println!();

    println!(
        "== [1/{}] commutativity + idempotence (baseline) ==",
        plan.len()
    );
    run("baseline", &base, &params, false);
    println!(
        "== [2/{}] commutativity + idempotence (sim-fix) ==",
        plan.len()
    );
    run("sim-fix", &base, &params, true);

    // ASSOCIATIVITY: merge(merge(A,B),C) vs merge(A,merge(B,C)).
    // The commutativity runs above never exercised this, and the verifier
    // reported 5 associativity violations alongside the 12 commutativity ones,
    // so "one change closes both defects" is unproven until this is measured.
    let assoc = |tag: &str, states: &[(String, ChatRoomStateV1)], simfix: bool| {
        let mut fails = 0usize;
        let mut trips = 0usize;
        let mut tally: std::collections::BTreeMap<String, usize> = Default::default();
        let n = states.len();
        for i in 0..n {
            for j in 0..n {
                for k in 0..n {
                    if i == j || j == k || i == k {
                        continue;
                    }
                    let (mut a, mut b, mut c) = (
                        states[i].1.clone(),
                        states[j].1.clone(),
                        states[k].1.clone(),
                    );
                    if simfix {
                        // Same stand-in as above, using the A-B merged context.
                        let (active, banned) = merged_context(&a, &b, &params);
                        sweep_with(&mut a, &params, &active, &banned);
                        sweep_with(&mut b, &params, &active, &banned);
                        sweep_with(&mut c, &params, &active, &banned);
                    }
                    trips += 1;
                    let ab = match merge(&a, &b, &params) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let bc = match merge(&b, &c, &params) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let left = merge(&ab, &c, &params);
                    let right = merge(&a, &bc, &params);
                    if let (Ok(l), Ok(r)) = (left, right) {
                        if ser(&l) != ser(&r) {
                            fails += 1;
                            *tally.entry(diff_fields(&l, &r).join("+")).or_default() += 1;
                        }
                    }
                }
            }
        }
        println!("[{tag}] associativity: {fails} failing / {trips} ordered triples");
        if !tally.is_empty() {
            println!("[{tag}] differing-field tally: {tally:?}");
        }
    };
    println!("== [3/{}] associativity (baseline) ==", plan.len());
    assoc("baseline-assoc", &base, false);
    println!("== [4/{}] associativity (sim-fix) ==", plan.len());
    assoc("sim-fix-assoc", &base, true);

    println!("\nall {} checks complete", plan.len());
}
