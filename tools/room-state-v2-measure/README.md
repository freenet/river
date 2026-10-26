# room-state-v2-measure

Times room state v2's `validate_state`, `summarize` and merge on the WASM
build, under the engine settings freenet-core runs contracts with
(`crates/core/src/wasm_runtime/engine/wasmtime_engine.rs`, `create_engine`):
wasmtime 47.0.3, `cranelift_opt_level(None)`, a 256 MiB memory cap with a
matching reservation and a one-page guard, an 8 MiB wasm stack, epoch
interruption on, fuel off. The guest is built with the room contract's release
profile (`opt-level = "z"`, LTO). A fresh instance runs each call. freenet-core
gives each call 5 s (`max_execution_seconds`) and caps a state at 50 MiB
(`MAX_STATE_SIZE`).

The states come from `river_core::room_state_v2::bench::largest_state`: every
cap full, every record at its largest size, two variants that share only the
L1 records, so their union holds twice the records `f` chooses between.

```bash
# From the river repo root (the pinned toolchain), into any target dir:
cargo build --manifest-path tools/room-state-v2-measure/Cargo.toml \
  -p guest --release --target wasm32-unknown-unknown --target-dir <dir>
cargo build --manifest-path tools/room-state-v2-measure/Cargo.toml \
  -p host --profile host --target-dir <dir>
REPS=3 <dir>/host/host <dir>/wasm32-unknown-unknown/release/guest.wasm \
  official proposed 40 20 10 4 2 1
```

A spec is `official` (the Official room's current contents in v2 records),
`proposed` (the candidate ceilings, `Caps::proposed`), or a divisor `d` of the
placeholder ceilings of the design's bounds table (`1` = the ceilings).

The calls: `validate` is `validate_state`; `validate_no_signatures` the same
with every signature taken as verified; `update_with_state` is the held state
plus another state's records it lacks, each new record checked
(`apply_delta`); `merge_untrusted` is `f(A ∪ B)` with nothing trusted.

## Results, 2026-10-26, nova (16 cores, load average 12 to 15)

Per primitive: `verify_strict` 0.36 to 0.38 ms per signature over a 100-byte
message (0.27 ms with `opt-level = 3`); BLAKE3 about 3 ms per MiB.

| state | records | signatures | bytes | validate | validate, no signatures | update with a state | merge, untrusted |
|---|---|---|---|---|---|---|---|
| official | 1,496 | 1,918 | 3.5 MB | 0.89 to 0.99 s | 0.29 to 0.45 s | 0.83 to 0.91 s | 1.08 to 1.16 s |
| proposed | 2,962 | 4,002 | 6.3 MB | 1.73 to 1.82 s | 0.43 to 0.46 s | 1.51 to 1.59 s | 1.95 to 2.25 s |
| ceilings / 40 | 1,925 | 3,925 | 6.3 MB | 2.1 s | 0.51 s | 1.9 to 2.7 s | 2.8 to 4.0 s |
| ceilings / 20 | 3,525 | 7,525 | 9.9 MB | 3.7 s | 0.83 s | 4.0 to 4.8 s | 4.8 to 5.0 s |
| ceilings / 10 | 6,725 | 14,725 | 17.1 MB | 8.8 s | 1.23 s | 6.6 s | 9.3 s |
| ceilings / 4 | 16,325 | 36,325 | 38.5 MB | 14.5 s | 2.5 s | out of memory | out of memory |
| ceilings / 2 | 32,325 | 72,325 | 74.3 MB | out of memory | | | |
| ceilings | 64,326 | 144,326 | 145.8 MB | out of memory | | | |

Signatures dominate: about 0.4 to 0.5 ms each, including hashing the signed
bytes (a 32 KiB DM is a 32 KiB SHA-512). Everything else costs about 70 ms per
MB of state. At the placeholder ceilings the largest state is 145.8 MB (the
computed byte ceiling, which allows for legacy records at their largest, is
550 MB), above both `MAX_STATE_SIZE` and the 256 MiB memory cap.
