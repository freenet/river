//! Builds the largest valid state for a scale of the placeholder ceilings
//! (`Caps::ceilings().scaled(d)`), two variants of it, and times the guest's
//! entry points under freenet-core's contract engine settings:
//! `cranelift_opt_level(None)`, a 256 MiB memory cap (the `ResourceLimiter`
//! freenet-core installs, `DEFAULT_MAX_MEMORY_PAGES` = 4096) with a matching
//! reservation and one-page guard, an 8 MiB wasm stack, epoch interruption
//! on, fuel off. Usage: `host <guest.wasm> <spec>...`, a spec being a divisor of the
//! ceilings (1 = the ceilings), `official`, `ceilings`, `legacy`, `membersN`,
//! `lmembersN[:L]` (see `Caps`) or `oshaped` (see [`oshaped`]). With `CHECK_SECS=s`,
//! exits non-zero if any validate, update or merge call fails or takes longer
//! than `s` seconds (the local check of the ceilings: `CHECK_SECS=2.5 host
//! guest.wasm ceilings legacy`). `REPS=n`
//! repeats each call and reports every run.

use river_core::room_state_v2::bench::{largest_state, owner_key, Caps};
use river_core::room_state_v2::encoding::Bytes32;
use std::time::Instant;
use wasmtime::*;

const PAGES: u64 = 4096;
const PAGE: u64 = 65536;

struct Host {
    limits: StoreLimits,
}

fn engine() -> Engine {
    let mut c = Config::new();
    c.epoch_interruption(true);
    c.max_wasm_stack(8 * 1024 * 1024);
    c.async_stack_size(16 * 1024 * 1024);
    c.memory_reservation(PAGES * PAGE);
    c.memory_guard_size(PAGE);
    c.memory_reservation_for_growth(0);
    // freenet-core uses None; OPT=speed or OPT=speed_and_size measures the
    // alternatives with everything else unchanged.
    let opt = match std::env::var("OPT").as_deref() {
        Ok("speed") => OptLevel::Speed,
        Ok("speed_and_size") => OptLevel::SpeedAndSize,
        _ => OptLevel::None,
    };
    println!("engine cranelift_opt_level {opt:?}");
    c.cranelift_opt_level(opt);
    // PROFILE=perfmap writes /tmp/perf-<pid>.map for `perf record` (build
    // the guest with `--profile guest-prof` to keep its function names).
    if std::env::var("PROFILE").as_deref() == Ok("perfmap") {
        c.profiler(ProfilingStrategy::PerfMap);
    }
    Engine::new(&c).unwrap()
}

struct Guest {
    store: Store<Host>,
    inst: Instance,
    mem: Memory,
}

impl Guest {
    fn new(engine: &Engine, module: &Module) -> Guest {
        let mut store = Store::new(
            engine,
            Host {
                limits: StoreLimitsBuilder::new()
                    .memory_size((PAGES * PAGE) as usize)
                    .build(),
            },
        );
        store.limiter(|h| &mut h.limits);
        store.set_epoch_deadline(u64::MAX / 2);
        let inst = Instance::new(&mut store, module, &[]).unwrap();
        let mem = inst.get_memory(&mut store, "memory").unwrap();
        Guest { store, inst, mem }
    }

    fn put(&mut self, bytes: &[u8]) -> Result<i32> {
        let alloc = self
            .inst
            .get_typed_func::<i32, i32>(&mut self.store, "alloc")?;
        let p = alloc.call(&mut self.store, bytes.len() as i32)?;
        self.mem.write(&mut self.store, p as usize, bytes)?;
        Ok(p)
    }

    fn checks(&mut self) -> i64 {
        let f = self
            .inst
            .get_typed_func::<(), i64>(&mut self.store, "checks")
            .unwrap();
        f.call(&mut self.store, ()).unwrap()
    }
}

/// Calls that took longer than `CHECK_SECS`, when it is set.
static OVER: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn report(label: &str, r: Result<i64>, t: Instant, g: &mut Guest) {
    let el = t.elapsed();
    let limit: Option<f64> = std::env::var("CHECK_SECS")
        .ok()
        .and_then(|x| x.parse().ok());
    if let Some(l) = limit {
        if label != "validate, no signatures" && label != "summarize_state" {
            if r.is_err() {
                OVER.lock().unwrap().push(format!("{label}: failed"));
            } else if el.as_secs_f64() > l {
                OVER.lock()
                    .unwrap()
                    .push(format!("{label}: {:.3} s > {l} s", el.as_secs_f64()));
            }
        }
    }
    match r {
        Ok(n) => println!(
            "  {label:<22} {:>9.3} s  result {n:>7}  sig checks {:>7}  memory {} MiB",
            el.as_secs_f64(),
            g.checks(),
            g.mem.data_size(&g.store) >> 20
        ),
        Err(e) => println!("  {label:<22} {:>9.3} s  FAILED: {e:#}", el.as_secs_f64()),
    }
}

/// The signatures a record carries.
fn signatures(r: &river_core::room_state_v2::Record) -> usize {
    use river_core::room_state_v2::records::{ChainLink, Record};
    match r {
        Record::Link(_) => 3,
        Record::Restore(s) => {
            1 + s
                .payload
                .chain
                .iter()
                .map(|c| match c {
                    ChainLink::Accepted(_) => 3,
                    ChainLink::Legacy(_) => 1,
                })
                .sum::<usize>()
        }
        _ => 1,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let wasm = args.next().expect("guest.wasm");
    let specs: Vec<String> = args.collect();
    let engine = engine();
    let module = Module::from_file(&engine, &wasm).unwrap();
    let o = Bytes32(owner_key().verifying_key().to_bytes());
    {
        let mut g = Guest::new(&engine, &module);
        let f: TypedFunc<i32, i64> = g.inst.get_typed_func(&mut g.store, "verify_n").unwrap();
        let t = Instant::now();
        let r = f.call(&mut g.store, 200);
        let el = t.elapsed().as_secs_f64();
        println!(
            "verify_strict: {:.3} ms per signature ({:?} of 200 verified)",
            el * 1000.0 / 200.0,
            r
        );
        let buf = vec![0xabu8; 16 << 20];
        let p = g.put(&buf).unwrap();
        let h: TypedFunc<(i32, i32), i64> = g.inst.get_typed_func(&mut g.store, "hash_n").unwrap();
        let t = Instant::now();
        h.call(&mut g.store, (p, buf.len() as i32)).unwrap();
        println!(
            "blake3: {:.1} ms per MiB",
            t.elapsed().as_secs_f64() * 1000.0 / 16.0
        );
    }
    for d in specs {
        let caps = match d.as_str() {
            "official" => Caps::official_room(),
            "ceilings" => Caps::ceilings(),
            "legacy" => Caps::ceilings_legacy(),
            n if n.starts_with("members") => Caps::members(n[7..].parse().unwrap()),
            n if n.starts_with("lmembers") => {
                // `lmembersN` or `lmembersN:L` (L legacy links, at most
                // MAX_LEGACY_LINKS; default min(N, MAX_LEGACY_LINKS)).
                let (m, l) = match n[8..].split_once(':') {
                    Some((m, l)) => (m.parse().unwrap(), l.parse().unwrap()),
                    None => {
                        let m: u32 = n[8..].parse().unwrap();
                        (m, m)
                    }
                };
                Caps {
                    legacy: true,
                    legacy_links: l
                        .min(river_core::room_state_v2::config::ceiling::MAX_LEGACY_LINKS),
                    ..Caps::members(m)
                }
            }
            "oshaped" => {
                oshaped(&engine, &module);
                continue;
            }
            "adv" => {
                adversarial(&engine, &module);
                continue;
            }
            "mixed" => Caps {
                legacy: true,
                legacy_links: 0,
                ..Caps::ceilings()
            },
            "pipeline" => {
                pipeline(&engine, &module);
                continue;
            }
            "slack" => {
                slack(&engine, &module);
                continue;
            }
            "purgejunk" => {
                purge_junk(&engine, &module);
                continue;
            }
            "purgecheck" => {
                purge_check(&engine, &module);
                continue;
            }
            n => Caps::ceilings().scaled(n.parse().unwrap()),
        };
        let t = Instant::now();
        let (a, da) = largest_state(caps, 0);
        let (b, db) = largest_state(caps, 1);
        assert_eq!((da, db), (0, 0), "the builder made a state f changes");
        let (ab, bb) = (a.to_bytes(), b.to_bytes());
        println!(
            "{d}: {} records, {} signatures, {} bytes (variant 2: {} bytes); built natively in {:.1} s; caps {caps:?}",
            a.records.len(),
            a.records.iter().map(signatures).sum::<usize>(),
            ab.len(),
            bb.len(),
            t.elapsed().as_secs_f64()
        );
        run(&engine, &module, &o, &ab, &bb, &ab);
    }
    let over = OVER.lock().unwrap();
    if std::env::var("CHECK_SECS").is_ok() {
        if over.is_empty() {
            println!("CHECK PASSED: every validate and merge within CHECK_SECS");
        } else {
            println!("CHECK FAILED:");
            for o in over.iter() {
                println!("  {o}");
            }
            std::process::exit(1);
        }
    }
}

/// Times every entry point `REPS` times, a fresh instance per call:
/// validate and summarize `ab`, update the held state `held` with `bb`, and
/// merge `ab` with `bb`.
fn run(engine: &Engine, module: &Module, o: &Bytes32, ab: &[u8], bb: &[u8], held: &[u8]) {
    let reps: usize = std::env::var("REPS")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(1);
    // ONLY=<call> (e.g. ONLY=merge_untrusted) runs just that call, for profiling.
    let only = std::env::var("ONLY").ok();
    let want = |n: &str| only.as_deref().is_none_or(|o| o == n);
    for _ in 0..reps {
        // A fresh instance per call, as freenet-core instantiates per call.
        type F3 = TypedFunc<(i32, i32, i32), i64>;
        type F5 = TypedFunc<(i32, i32, i32, i32, i32), i64>;
        if want("validate_state") {
            let mut g = Guest::new(engine, module);
            let (po, pa) = (g.put(&o.0).unwrap(), g.put(ab).unwrap());
            let f: F3 = g.inst.get_typed_func(&mut g.store, "validate").unwrap();
            let t = Instant::now();
            let r = f.call(&mut g.store, (po, pa, ab.len() as i32));
            report("validate_state", r, t, &mut g);
        }
        if want("validate_no_signatures") {
            let mut g = Guest::new(engine, module);
            let (po, pa) = (g.put(&o.0).unwrap(), g.put(ab).unwrap());
            let f: F3 = g
                .inst
                .get_typed_func(&mut g.store, "validate_no_signatures")
                .unwrap();
            let t = Instant::now();
            let r = f.call(&mut g.store, (po, pa, ab.len() as i32));
            report("validate, no signatures", r, t, &mut g);
        }
        if want("summarize_state") {
            let mut g = Guest::new(engine, module);
            let po = g.put(&o.0).unwrap();
            let summ: TypedFunc<(i32, i32), i64> =
                g.inst.get_typed_func(&mut g.store, "summarize").unwrap();
            let pa = g.put(ab).unwrap();
            let t = Instant::now();
            let r = summ.call(&mut g.store, (pa, ab.len() as i32));
            report("summarize_state", r, t, &mut g);
            let _ = po;
        }
        for name in ["update_with_state", "merge_untrusted"] {
            if !want(name) {
                continue;
            }
            let mut g = Guest::new(engine, module);
            let po = g.put(&o.0).unwrap();
            let first = if name == "update_with_state" {
                held
            } else {
                ab
            };
            let pa = match g.put(first) {
                Ok(p) => p,
                Err(e) => {
                    println!("  {name:<22} could not load inputs: {e:#}");
                    OVER.lock()
                        .unwrap()
                        .push(format!("{name}: inputs did not load"));
                    continue;
                }
            };
            let pb = match g.put(bb) {
                Ok(p) => p,
                Err(e) => {
                    println!("  {name:<22} could not load inputs: {e:#}");
                    OVER.lock()
                        .unwrap()
                        .push(format!("{name}: inputs did not load"));
                    continue;
                }
            };
            let f: F5 = g.inst.get_typed_func(&mut g.store, name).unwrap();
            let t = Instant::now();
            let r = f.call(
                &mut g.store,
                (po, pa, first.len() as i32, pb, bb.len() as i32),
            );
            report(name, r, t, &mut g);
        }
    }
}

/// `oshaped`: the Official-shaped v1 room (`official_shaped(1, 1.0)`, real
/// signatures) converted by its owner, the bot's deputies confirmed and the
/// bot protected. Validate and summarize the whole state; merge it with
/// itself; update a state holding only the owner's records with it (a peer
/// joining: every legacy record checked).
fn oshaped(engine: &Engine, module: &Module) {
    use river_core::room_state_v2::convert::{legacy_records, owner_payloads};
    use river_core::room_state_v2::encoding::Key;
    use river_core::room_state_v2::official_shaped::official_shaped;
    use river_core::room_state_v2::{normalize, Verifier};
    let f = official_shaped(1, 1.0);
    let k = Key::from(&f.owner.verifying_key());
    let legacy = legacy_records(&k, &f.v1).records;
    let deputies: Vec<Key> = f.deputies.iter().map(Key::from).collect();
    let p = owner_payloads(
        &k,
        &f.v1,
        &legacy,
        24 * 300,
        &deputies,
        &[Key::from(&f.bot)],
    )
    .unwrap()
    .payloads;
    let owner_recs = p.sign(&f.owner);
    let mut recs = owner_recs.clone();
    recs.extend(legacy);
    let full = normalize(&k, recs, &mut Verifier::new()).0;
    let held = normalize(&k, owner_recs, &mut Verifier::new()).0;
    let (fb, hb) = (full.to_bytes(), held.to_bytes());
    println!(
        "oshaped: {} records, {} signatures, {} bytes; held for the update: {} records",
        full.records.len(),
        full.records.iter().map(signatures).sum::<usize>(),
        fb.len(),
        held.records.len()
    );
    run(engine, module, &k, &fb, &fb, &hb);
}

/// `adv`: the worst adversarial update found: the held state is the largest
/// mixed legacy state, the update every record of the other variant except
/// the owner's (configuration, clock, checkpoint, upgrade, roster, secret
/// versions). It holds as many records of each kind as a valid state, each
/// decoded, and `f` drops almost all of them without a signature check
/// (their members are not admitted). Only update and merge are meaningful.
fn adversarial(engine: &Engine, module: &Module) {
    let caps = Caps::ceilings_legacy();
    let (a, _) = largest_state(caps, 0);
    let (b, _) = largest_state(caps, 1);
    let o = Bytes32(owner_key().verifying_key().to_bytes());
    let junk = river_core::room_state_v2::RoomStateV2::from_records(
        b.records
            .into_iter()
            .filter(|r| !matches!(r.kind(), 0 | 2 | 3 | 4 | 5 | 6))
            .collect(),
    );
    let (ab, jb) = (a.to_bytes(), junk.to_bytes());
    println!(
        "adv: held {} records ({} bytes); update {} records ({} bytes)",
        a.records.len(),
        ab.len(),
        junk.records.len(),
        jb.len()
    );
    run(engine, module, &o, &ab, &jb, &ab);
}

/// The states the costlier specs start from: every cap full with v2 links
/// (`ceilings`), with 300 legacy links and legacy content (`legacy`), and
/// with v2 links and legacy content (`mixed`: the most signatures and most
/// v1 decoding together).
fn bases() -> Vec<(&'static str, Caps)> {
    vec![
        ("ceilings", Caps::ceilings()),
        ("legacy", Caps::ceilings_legacy()),
        (
            "mixed",
            Caps {
                legacy: true,
                legacy_links: 0,
                ..Caps::ceilings()
            },
        ),
    ]
}

/// A new owner message, newer than every message `s` holds, of the room's
/// privacy: the one-record delta of an ordinary post.
fn owner_post(s: &river_core::room_state_v2::RoomStateV2) -> Vec<u8> {
    use river_core::room_state_v2::config::EPOCH_2026_SECS;
    use river_core::room_state_v2::encoding::{sign_payload, to_cbor, ByteBuf, Key, Nonce12};
    use river_core::room_state_v2::records::*;
    let osk = owner_key();
    let o = Key::from(&osk.verifying_key());
    let clock = s
        .records
        .iter()
        .find_map(|r| match r {
            Record::Clock(c) => Some(c.payload.hours),
            _ => None,
        })
        .unwrap_or(0);
    let private = s
        .records
        .iter()
        .any(|r| matches!(r, Record::Configuration(c) if c.payload.privacy_mode == 1));
    let body = if private {
        MessageBody::Private(PrivateBody {
            content_type: 1,
            content_version: 1,
            ciphertext: ByteBuf(vec![0xab; 1_000]),
            nonce: Nonce12([9; 12]),
            secret_version: 1,
        })
    } else {
        MessageBody::Public(PublicBody {
            content_type: 1,
            content_version: 1,
            data: ByteBuf(vec![0xab; 1_000]),
        })
    };
    let p = MessageP {
        author: o,
        time: EPOCH_2026_SECS + clock as u64 * 3600 + 1_800,
        clock_ref: clock,
        body,
    };
    to_cbor(&vec![Record::Message(Signed {
        sig: sign_payload(tag::MESSAGE, &o, &p, &osk),
        payload: p,
    })])
}

/// `pipeline`: one ordinary post on a full state, as freenet-core handles
/// it: apply, validate the result, summarize it, and the delta back to a
/// subscriber holding the old state (guest `pipeline`), timed together; and
/// `get_state_delta` of one variant against the other's summary (the
/// largest delta a subscriber can be sent).
fn pipeline(engine: &Engine, module: &Module) {
    use river_core::room_state_v2::encoding::to_cbor;
    let o = Bytes32(owner_key().verifying_key().to_bytes());
    let reps: usize = std::env::var("REPS")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(1);
    for (name, caps) in bases() {
        let (a, _) = largest_state(caps, 0);
        let (b, _) = largest_state(caps, 1);
        let (ab, post) = (a.to_bytes(), owner_post(&a));
        let (bb, sa) = (b.to_bytes(), to_cbor(&a.summarize()));
        println!(
            "pipeline {name}: held {} records ({} bytes); a one-record delta of {} bytes",
            a.records.len(),
            ab.len(),
            post.len()
        );
        for _ in 0..reps {
            type F5 = TypedFunc<(i32, i32, i32, i32, i32), i64>;
            type F4 = TypedFunc<(i32, i32, i32, i32), i64>;
            let mut g = Guest::new(engine, module);
            let (po, pa, pd) = (
                g.put(&o.0).unwrap(),
                g.put(&ab).unwrap(),
                g.put(&post).unwrap(),
            );
            let f: F5 = g.inst.get_typed_func(&mut g.store, "pipeline").unwrap();
            let t = Instant::now();
            let r = f.call(
                &mut g.store,
                (po, pa, ab.len() as i32, pd, post.len() as i32),
            );
            report("update+validate+summ", r, t, &mut g);

            let mut g = Guest::new(engine, module);
            let (pb, ps) = (g.put(&bb).unwrap(), g.put(&sa).unwrap());
            let f: F4 = g.inst.get_typed_func(&mut g.store, "state_delta").unwrap();
            let t = Instant::now();
            let r = f.call(&mut g.store, (pb, bb.len() as i32, ps, sa.len() as i32));
            report("get_state_delta", r, t, &mut g);
        }
    }
}

/// The most bytes a delta may carry, less a margin for its encoding.
fn delta_budget() -> usize {
    river_core::room_state_v2::max_state_bytes() - 4_096
}

/// `slack`: the `mixed` state updated with legacy DMs over the content
/// bound, filling the delta's byte allowance: the window is open, so each
/// is decoded (element by element, as v1 encodes bytes) before its size
/// drops it, with no signature check.
fn slack(engine: &Engine, module: &Module) {
    use river_core::room_state::direct_messages::sign_direct_message;
    use river_core::room_state::member::MemberId;
    use river_core::room_state_v2::encoding::{to_cbor, ByteBuf, Key};
    use river_core::room_state_v2::records::{Legacy, Record};
    let (_, caps) = bases().remove(2);
    let (a, _) = largest_state(caps, 0);
    let osk = owner_key();
    let ovk = osk.verifying_key();
    let other = ed25519_dalek::SigningKey::from_bytes(&[0x77; 32]).verifying_key();
    let n = river_core::room_state_v2::config::ceiling::MAX_DIRECT_MESSAGES as usize;
    let len = delta_budget() / n - 200;
    let junk: Vec<Record> = (0..n)
        .map(|i| {
            let dm = sign_direct_message(
                &osk,
                MemberId::from(&ovk),
                MemberId::from(&other),
                &ovk,
                1_800_000_000 + i as u64,
                vec![1u8; len],
            )
            .unwrap();
            Record::LegacyDirectMessage(Legacy {
                signer: Key::from(&ovk),
                v1: ByteBuf(to_cbor(&dm)),
            })
        })
        .collect();
    let jb = river_core::room_state_v2::RoomStateV2::from_records(junk).to_bytes();
    let ab = a.to_bytes();
    let o = Bytes32(ovk.to_bytes());
    println!(
        "slack: held {} records ({} bytes); update {n} legacy DMs of {len} bytes ({} bytes)",
        a.records.len(),
        ab.len(),
        jb.len()
    );
    run(engine, module, &o, &ab, &jb, &ab);
}

/// `purgejunk`: the `mixed` state updated with legacy purge envelopes of
/// 1,000 tokens each, as many as the delta's bytes allow: each is decoded
/// before its recipient is looked at.
fn purge_junk(engine: &Engine, module: &Module) {
    use river_core::room_state::direct_messages::{
        sign_recipient_purges, PurgeToken, RecipientPurges,
    };
    use river_core::room_state::member::MemberId;
    use river_core::room_state_v2::encoding::{to_cbor, ByteBuf, Key};
    use river_core::room_state_v2::records::{Legacy, Record};
    let (_, caps) = bases().remove(2);
    let (a, _) = largest_state(caps, 0);
    let osk = owner_key();
    let ovk = osk.verifying_key();
    let one = |v: u64| {
        let p = sign_recipient_purges(
            &osk,
            MemberId::from(&ovk),
            &ovk,
            RecipientPurges {
                version: v,
                purged: (0..1_000u32)
                    .map(|t| {
                        let mut b = [0xffu8; 16];
                        b[..4].copy_from_slice(&t.to_le_bytes());
                        b[4..12].copy_from_slice(&v.to_le_bytes());
                        PurgeToken(b)
                    })
                    .collect(),
            },
        )
        .unwrap();
        Record::LegacyPurge(Legacy {
            signer: Key::from(&ovk),
            v1: ByteBuf(to_cbor(&p)),
        })
    };
    let size = to_cbor(&one(1)).len();
    let n = (delta_budget() / size)
        .min(river_core::room_state_v2::config::ceiling::MAX_MEMBERS as usize + 1);
    let junk: Vec<Record> = (1..=n as u64).map(one).collect();
    let jb = river_core::room_state_v2::RoomStateV2::from_records(junk).to_bytes();
    let ab = a.to_bytes();
    let o = Bytes32(ovk.to_bytes());
    println!(
        "purgejunk: held {} records ({} bytes); update {n} legacy purge envelopes of 1,000 tokens ({} bytes)",
        a.records.len(),
        ab.len(),
        jb.len()
    );
    run(engine, module, &o, &ab, &jb, &ab);
}

/// `purgecheck`: the ceilings state updated with, and merged with, the same
/// state built with every purge envelope at 1,000 tokens (401 of them, five
/// times the budget): each recipient's candidate is checked, then most are
/// refused by the budget (the extra checks of decision (t)).
fn purge_check(engine: &Engine, module: &Module) {
    use river_core::room_state_v2::bench::largest_records_over_purge_budget;
    let caps = Caps::ceilings();
    let (a, _) = largest_state(caps, 0);
    let over = river_core::room_state_v2::RoomStateV2::from_records(
        largest_records_over_purge_budget(caps, 0),
    );
    let (ab, ob) = (a.to_bytes(), over.to_bytes());
    let o = Bytes32(owner_key().verifying_key().to_bytes());
    println!(
        "purgecheck: held {} records ({} bytes); update {} records ({} bytes)",
        a.records.len(),
        ab.len(),
        over.records.len(),
        ob.len()
    );
    run(engine, module, &o, &ab, &ob, &ab);
}
