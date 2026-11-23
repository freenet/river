//! Runs a river room contract WASM under wasmtime the way freenet-core's
//! runtime does (fresh instance per call, streaming input buffers of 64 KiB
//! refilled through `__frnt__fill_buffer`, bincode-framed results) and times
//! `update_state` with a one-message delta followed by `validate_state` of its
//! output.
//!
//! usage: wasmbench <contract.wasm> <params.cbor> <state.cbor> <delta.cbor> [iterations]
//! See README.md for how to produce the fixture files.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use wasmtime::{bail, format_err, Caller, Engine, Instance, Linker, Memory, Module, Result, Store, TypedFunc};

const STREAMING_BUF_CAP: usize = 64 * 1024;

#[derive(Default)]
struct Host {
    pending: HashMap<i64, (Vec<u8>, usize)>,
}

fn rd32(mem: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(mem[at..at + 4].try_into().unwrap())
}
fn rd64(mem: &[u8], at: usize) -> i64 {
    i64::from_le_bytes(mem[at..at + 8].try_into().unwrap())
}
fn wr32(mem: &mut [u8], at: usize, v: u32) {
    mem[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

struct Builder {
    start: usize,
    capacity: usize,
    last_read: usize,
    last_write: usize,
}

fn builder(mem: &[u8], ptr: i64) -> Builder {
    let p = ptr as usize;
    Builder {
        start: rd64(mem, p) as usize,
        capacity: rd32(mem, p + 8) as usize,
        last_read: rd64(mem, p + 16) as usize,
        last_write: rd64(mem, p + 24) as usize,
    }
}

struct Running {
    store: Store<Host>,
    instance: Instance,
    memory: Memory,
    init_buf: TypedFunc<u32, i64>,
}

fn instantiate(engine: &Engine, module: &Module, linker: &Linker<Host>) -> Result<Running> {
    let mut store = Store::new(engine, Host::default());
    let instance = linker.instantiate(&mut store, module)?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| format_err!("no memory export"))?;
    let init_buf = instance.get_typed_func::<u32, i64>(&mut store, "__frnt__initiate_buffer")?;
    if let Ok(set_id) = instance.get_typed_func::<i64, ()>(&mut store, "__frnt_set_id") {
        set_id.call(&mut store, 1)?;
    }
    Ok(Running {
        store,
        instance,
        memory,
        init_buf,
    })
}

impl Running {
    /// `write_streaming_buf`: a buffer of at most 64 KiB holding
    /// `[total_len: u32]` and the first chunk, the rest left for refills.
    fn streaming_buf(&mut self, data: &[u8]) -> Result<i64> {
        let cap = STREAMING_BUF_CAP.min(data.len() + 4);
        let ptr = self.init_buf.call(&mut self.store, cap as u32)?;
        let mem = self.memory.data_mut(&mut self.store);
        let b = builder(mem, ptr);
        assert!(b.capacity >= cap);
        mem[b.start..b.start + 4].copy_from_slice(&(data.len() as u32).to_le_bytes());
        let first = data.len().min(cap - 4);
        mem[b.start + 4..b.start + 4 + first].copy_from_slice(&data[..first]);
        wr32(mem, b.last_write, (4 + first) as u32);
        wr32(mem, b.last_read, 0);
        if first < data.len() {
            self.store
                .data_mut()
                .pending
                .insert(ptr, (data[first..].to_vec(), 0));
        }
        Ok(ptr)
    }

    fn call3(&mut self, name: &str, a: i64, b: i64, c: i64) -> Result<Vec<u8>> {
        let f = self
            .instance
            .get_typed_func::<(i64, i64, i64), i64>(&mut self.store, name)?;
        let res_ptr = f.call(&mut self.store, (a, b, c))?;
        let mem = self.memory.data(&self.store);
        let p = res_ptr as usize;
        let data_ptr = rd64(mem, p) as usize;
        let size = rd32(mem, p + 12) as usize;
        Ok(mem[data_ptr..data_ptr + size].to_vec())
    }
}

fn linker(engine: &Engine) -> Result<Linker<Host>> {
    let mut linker = Linker::new(engine);
    linker.func_wrap(
        "freenet_contract_io",
        "__frnt__fill_buffer",
        |mut caller: Caller<'_, Host>, _id: i64, buf_ptr: i64| -> u32 {
            let mem = caller.get_export("memory").unwrap().into_memory().unwrap();
            let (rest, cursor) = match caller.data().pending.get(&buf_ptr) {
                Some((d, c)) if *c < d.len() => (d[*c..].to_vec(), *c),
                _ => return 0,
            };
            let data = mem.data_mut(&mut caller);
            let b = builder(data, buf_ptr);
            let n = rest.len().min(b.capacity);
            wr32(data, b.last_read, 0);
            data[b.start..b.start + n].copy_from_slice(&rest[..n]);
            wr32(data, b.last_write, n as u32);
            caller.data_mut().pending.get_mut(&buf_ptr).unwrap().1 = cursor + n;
            n as u32
        },
    )?;
    Ok(linker)
}

/// bincode of `&[UpdateData::Delta(delta)]`.
fn update_data_delta(delta: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&1u64.to_le_bytes()); // one element
    out.extend_from_slice(&1u32.to_le_bytes()); // variant Delta
    out.extend_from_slice(&(delta.len() as u64).to_le_bytes());
    out.extend_from_slice(delta);
    out
}

/// bincode `Result<UpdateModification, ContractError>` -> the new state.
fn new_state_of(res: &[u8]) -> Result<Vec<u8>> {
    let tag = u32::from_le_bytes(res[..4].try_into()?);
    if tag != 0 {
        bail!("update_state returned an error: {:?}", String::from_utf8_lossy(&res[4..res.len().min(300)]));
    }
    if res[4] != 1 {
        bail!("update_state returned no new state");
    }
    let len = u64::from_le_bytes(res[5..13].try_into()?) as usize;
    Ok(res[13..13 + len].to_vec())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        bail!("usage: wasmbench contract.wasm params.cbor state.cbor delta.cbor [iterations]");
    }
    let wasm = std::fs::read(&args[1])?;
    let params = std::fs::read(&args[2])?;
    let state = std::fs::read(&args[3])?;
    let delta = std::fs::read(&args[4])?;
    let iterations: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(10);

    let engine = Engine::default();
    let module = Module::new(&engine, &wasm)?;
    let linker = linker(&engine)?;

    let update = |state: &[u8]| -> Result<(Vec<u8>, Duration)> {
        let t = Instant::now();
        let mut r = instantiate(&engine, &module, &linker)?;
        let p = r.streaming_buf(&params)?;
        let s = r.streaming_buf(state)?;
        let u = r.streaming_buf(&update_data_delta(&delta))?;
        let res = r.call3("update_state", p, s, u)?;
        let out = new_state_of(&res)?;
        Ok((out, t.elapsed()))
    };
    let validate = |state: &[u8]| -> Result<Duration> {
        let t = Instant::now();
        let mut r = instantiate(&engine, &module, &linker)?;
        let p = r.streaming_buf(&params)?;
        let s = r.streaming_buf(state)?;
        // `RelatedContracts`: an empty map.
        let rel = r.streaming_buf(&0u64.to_le_bytes())?;
        let res = r.call3("validate_state", p, s, rel)?;
        let tag = u32::from_le_bytes(res[..4].try_into()?);
        let valid = u32::from_le_bytes(res[4..8].try_into()?);
        if tag != 0 || valid != 0 {
            bail!("validate_state refused: {:?}", String::from_utf8_lossy(&res));
        }
        Ok(t.elapsed())
    };

    // The input must be a valid state itself.
    validate(&state)?;
    let (warm, _) = update(&state)?; // warm up
    println!("update output byte-identical to the input state: {}", warm == state);
    let mut ups = Vec::new();
    let mut vals = Vec::new();
    let mut out_len = 0;
    for _ in 0..iterations {
        let (out, u) = update(&state)?;
        let v = validate(&out)?;
        out_len = out.len();
        ups.push(u);
        vals.push(v);
    }
    ups.sort();
    vals.sort();
    let (mu, mv) = (ups[ups.len() / 2], vals[vals.len() / 2]);
    println!(
        "wasm {} B, state {} B -> {} B: update_state {:.1?}  validate_state {:.1?}  pair {:.1?}  (median of {iterations}; min pair {:.1?})",
        wasm.len(),
        state.len(),
        out_len,
        mu,
        mv,
        mu + mv,
        ups[0] + vals[0]
    );
    Ok(())
}
