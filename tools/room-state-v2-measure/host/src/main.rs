//! Builds the largest valid state for a scale of the placeholder ceilings
//! (`Caps::ceilings().scaled(d)`), two variants of it, and times the guest's
//! entry points under freenet-core's contract engine settings:
//! `cranelift_opt_level(None)`, a 256 MiB memory cap (the `ResourceLimiter`
//! freenet-core installs, `DEFAULT_MAX_MEMORY_PAGES` = 4096) with a matching
//! reservation and one-page guard, an 8 MiB wasm stack, epoch interruption
//! on, fuel off. Usage: `host <guest.wasm> <spec>...`, a spec being a divisor of the
//! ceilings (1 = the ceilings), `official`, `ceilings` or `legacy` (see `Caps`). With `CHECK_SECS=s`,
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
            n if n.starts_with("lmembers") => Caps {
                legacy: true,
                ..Caps::members(n[8..].parse().unwrap())
            },
            n => Caps::ceilings().scaled(n.parse().unwrap()),
        };
        let reps: usize = std::env::var("REPS")
            .ok()
            .and_then(|x| x.parse().ok())
            .unwrap_or(1);
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
        for _ in 0..reps {
            // A fresh instance per call, as freenet-core instantiates per call.
            type F3 = TypedFunc<(i32, i32, i32), i64>;
            type F5 = TypedFunc<(i32, i32, i32, i32, i32), i64>;
            {
                let mut g = Guest::new(&engine, &module);
                let (po, pa) = (g.put(&o.0).unwrap(), g.put(&ab).unwrap());
                let f: F3 = g.inst.get_typed_func(&mut g.store, "validate").unwrap();
                let t = Instant::now();
                let r = f.call(&mut g.store, (po, pa, ab.len() as i32));
                report("validate_state", r, t, &mut g);
            }
            {
                let mut g = Guest::new(&engine, &module);
                let (po, pa) = (g.put(&o.0).unwrap(), g.put(&ab).unwrap());
                let f: F3 = g
                    .inst
                    .get_typed_func(&mut g.store, "validate_no_signatures")
                    .unwrap();
                let t = Instant::now();
                let r = f.call(&mut g.store, (po, pa, ab.len() as i32));
                report("validate, no signatures", r, t, &mut g);
            }
            {
                let mut g = Guest::new(&engine, &module);
                let po = g.put(&o.0).unwrap();
                let summ: TypedFunc<(i32, i32), i64> =
                    g.inst.get_typed_func(&mut g.store, "summarize").unwrap();
                let pa = g.put(&ab).unwrap();
                let t = Instant::now();
                let r = summ.call(&mut g.store, (pa, ab.len() as i32));
                report("summarize_state", r, t, &mut g);
                let _ = po;
            }
            for name in ["update_with_state", "merge_untrusted"] {
                let mut g = Guest::new(&engine, &module);
                let po = g.put(&o.0).unwrap();
                let pa = match g.put(&ab) {
                    Ok(p) => p,
                    Err(e) => {
                        println!("  {name:<22} could not load inputs: {e:#}");
                        OVER.lock()
                            .unwrap()
                            .push(format!("{name}: inputs did not load"));
                        continue;
                    }
                };
                let pb = match g.put(&bb) {
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
                let r = f.call(&mut g.store, (po, pa, ab.len() as i32, pb, bb.len() as i32));
                report(name, r, t, &mut g);
            }
        }
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
