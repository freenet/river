# wasm-cost-bench

Times the room contract WASM under wasmtime for freenet/river#974.

For each iteration it measures `update_state` with the given delta, then
`validate_state` of the resulting state. Every call uses a fresh instance, and
the inputs are passed through 64 KiB streaming buffers refilled by
`__frnt__fill_buffer`, as freenet-core's runtime does. It reports the median, p95 and max
of N iterations (use at least 100 for the p95 to mean anything) and fails if the
delta leaves the state unchanged, since a skipped delta would time a no-op.

This is a standalone crate. It is not a member of the root workspace (it has
its own empty `[workspace]` table), so it does not touch the root `Cargo.lock`
that pins the contract WASM build.

## Run

Write the fixtures with the native bench, then run this tool against the
contract WASM:

```text
CARGO_PROFILE_DEV_OPT_LEVEL=3 BENCH_DUMP_DIR=/tmp/room-fixtures \
  cargo bench -p river-core --bench provisional_pair --profile dev

cargo build --release --manifest-path tools/wasm-cost-bench/Cargo.toml
W=tools/wasm-cost-bench/target/release/wasmbench
C=ui/public/contracts/room_contract.wasm
F=/tmp/room-fixtures
$W $C $F/params.cbor $F/state.cbor $F/delta.cbor 100             # one message
$W $C $F/params.cbor $F/state.cbor $F/adversarial_delta.cbor 100 # worst-case delta
```

The state is a room at the provisional ceilings (24 entries, 24 bans, a full
100-message window). `delta.cbor` carries one message. `adversarial_delta.cbor`
carries 24 new provisional entries, 24 new provisional bans, and a
`member_info` record and a message for each new author.
