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

A spec is `ceilings` (every cap at its ceiling, all v2 records), `legacy` (the
same with owner bans, activity, member info, messages, DMs, purges and blobs as
legacy v1 records), `official` (the Official room's contents in v2 records),
`membersN` / `lmembersN` / `lmembersN:L` (see "How many members fit"; `L` is
the number of legacy links, default `min(N, MAX_LEGACY_LINKS)`), `oshaped`
(the Official-shaped v1 room converted by its owner: validate it, merge it
with itself, and update a state holding only the owner's records with it),
`adv` (the largest mixed legacy state updated with every non-owner record of
the other variant; only update and merge are meaningful), or a divisor `d` of
the ceilings. The `live` spec of the 2026-10-26 and 2026-10-27 results below
(a fetched v1 room, converted without its owner) was replaced by `oshaped`.

`ONLY=<call>` runs one call (`validate_state`, `update_with_state`, ...).
To profile, build the guest with `--profile guest-prof` (function names
kept) and run `PROFILE=perfmap ONLY=<call> perf record ./host ...`; the
guest's samples are under the `[JIT]` DSO in `perf report --sort dso,symbol`.

**The local check of the ceilings** (not in CI; run it on nova after any change
to `room_state_v2` or the ceilings):

```bash
CHECK_SECS=2.5 REPS=3 <dir>/host/host <dir>/wasm32-unknown-unknown/release/guest.wasm ceilings legacy
```

It exits non-zero if any `validate_state`, `update_with_state` or untrusted
merge of the union fails or takes longer than 2.5 s.

The sections below "Results" record the measurements that set the ceilings;
they were taken with the design's placeholder ceilings, which `official`,
`proposed` and the divisors then referred to.

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

## The engine's Cranelift opt level, 2026-10-26 (same machine and load)

`OPT=speed` or `OPT=speed_and_size` runs the host with that
`cranelift_opt_level`; nothing else changes (guest `opt-level = "z"`, 256 MiB
cap, epoch interruption). `members2000` is `Caps::members_2000()`: `proposed`
with 2,000 members, 4,000 links, the owner's child quota at 2,000, 8,000 blobs
(4 per member), a purge envelope per member, 400 owner and 400 deputy bans
(250 per deputy) and 100 member bans (20 per issuer). Times are the median of
three runs; per-signature cost is the 200-signature microbenchmark, per-MB cost
is `validate, no signatures` over the state's bytes.

| engine | per signature | per MB (no signatures) | official: validate / merge | proposed: validate / merge | members2000: validate / merge | ceilings / 4: validate / merge |
|---|---|---|---|---|---|---|
| None (freenet-core today) | 0.38 ms | 65 to 85 ms | 0.92 / 1.02 s | 1.59 / 1.90 s | 11.55 / 12.40 s | 15.2 / 21.5 s |
| Speed | 0.33 ms | 60 to 85 ms | 0.82 / 0.98 s | 1.65 / 1.93 s | 10.90 / 11.42 s | 13.1 / 18.4 s |
| SpeedAndSize | 0.34 ms | 60 to 85 ms | 0.78 / 0.90 s | 1.50 / 1.73 s | 10.53 / 11.21 s | 13.7 / 18.7 s |

Sizes: official 1,918 signatures and 3.0 MB; proposed 4,002 and 5.8 MB;
members2000 29,752 and 19.2 MB; ceilings / 4 36,325 and 38.1 MB (its union
merge now fits under the 256 MiB cap, at 255 MiB). The engine opt level buys 5
to 15 percent. One run of the guest at `opt-level = 3` (profile `guest-o3`,
engine None) gave proposed 1.41 / 1.51 s and members2000 11.1 / 10.3 s.

## The ceilings as shipped, 2026-10-26 (load average 7 to 9)

`config::ceiling` now holds the proposed caps (300 members, 400 links, 4 KiB DM
ciphertext, ...), and legacy records carrying content have their own bounds
(message or activity content 1,000 bytes, DM ciphertext 2,560 bytes; see
`ceiling::LEGACY_MESSAGE_SIZE`). `apply_delta` checks the signatures of new
records `f` did not keep after running `f`, so no record is decoded twice.

| state | records | signatures | bytes | validate | update with a state | merge, untrusted |
|---|---|---|---|---|---|---|
| ceilings | 2,901 | 3,941 | 3.65 MB | 1.49 to 1.63 s | 1.23 to 1.33 s | 1.55 to 2.21 s |
| legacy | 2,901 | 3,941 | 5.33 MB | 1.89 to 1.91 s | 2.15 to 2.23 s | 2.31 to 2.43 s |
| official | 1,435 | 1,857 | 0.86 MB | 0.64 to 0.65 s | 0.52 to 0.55 s | 0.68 to 0.71 s |

`CHECK_SECS=2.5`: passed. The legacy-heavy worst case has the least margin
(about 0.1 to 0.2 s), and nova was loaded; a slower peer will exceed 2.5 s
there, though not freenet-core's 5 s per call. Before the legacy bounds, the
legacy-heavy merge took 3.0 s and the update 3.2 s.

## Signature crates at opt-level 3, 2026-10-26

The guest (and the v2 room contract) now build curve25519-dalek,
ed25519-dalek, sha2 and blake3 at opt-level 3, everything else at "z".
`verify_strict` microbenchmark: 0.25 ms per signature (0.38 ms before). Same
check, load average 9 to 12:

| state | validate | update with a state | untrusted merge |
|---|---|---|---|
| ceilings | 1.08 s | 0.95 to 1.00 s | 1.17 to 1.28 s |
| legacy | 1.52 to 1.54 s | 1.93 to 1.96 s | 1.97 to 2.00 s |
| official | 0.47 s | 0.39 to 0.40 s | 0.50 s |

`CHECK_SECS=2.5`: passed.

### How many members fit

`membersN` / `lmembersN` are `Caps::members(N)`: the ceilings with links, the
owner's child quota and blobs at 4/3 x N and a purge envelope per member; the
`l` form carries the member-dependent records as legacy v1 records. Built with
`RUSTFLAGS="--cfg river_v2_wide_ceilings"` (guest and host) so the
configuration may exceed the shipped ceilings. Five runs each, minimum and
median, load average 12 to 30 (the larger counts ran under the heaviest load):

| members | signatures | v2: validate / update / merge (median) | legacy: validate / update / merge (median) |
|---|---|---|---|
| 300 | 3,966 | (see above) | 1.62-1.66 / 2.14-2.17 / 2.12-2.20 (3 runs) |
| 400 | 4,836 | 1.44 / 1.29 / 1.58 | 1.97 / 2.53 / 2.56 |
| 500 | 5,701 | 1.65 / 1.41 / 1.81 | 2.23 / 2.69 / 2.88 |
| 600 | 6,566 | 1.76 / 1.50 / 1.90 | 2.29 / 2.78 / 2.96 |
| 700 | 7,436 | 1.95 / 1.68 / 2.14 | |
| 800 | 8,301 | 2.20 / 1.83 / 2.41 | |
| 1,000 | 10,036 | min 2.89 / 2.29 / 3.65 | |

A room of v2 records stays under 2.5 s to about 800 members; a room whose
member-dependent records are all legacy v1 records (the worst case a migrated
room can reach) to about 300.

## 450 members, at most 300 legacy, 2026-10-26

Ian asked for 800 members with at most 300 legacy (migrated) members.
`legacy` is now the mixed worst case: 300 members joined by legacy links with
legacy activity, the rest by v2 links, and every member-dependent record a
legacy v1 record. `lmembersN` is that shape at N members (with
`--cfg river_v2_wide_ceilings`). Five runs, minimum to maximum, at load
average 4 to 7:

| members (300 legacy) | signatures | validate | update | merge |
|---|---|---|---|---|
| 300 | 3,354 | 1.40-1.44 | 1.94-2.26 | 1.80-1.93 |
| 400 | 4,224 | 1.49-1.76 | 2.02-2.35 | 2.01-2.41 |
| 450 | 4,654 | 1.72-1.77 | 2.16-2.24 | 2.20-2.33 |
| 500 | 5,089 | 1.83-1.95 | 2.27-2.32 | 2.33-2.47 |
| 550 | 5,524 | 2.13-2.45 | 2.63-3.68 | 2.67-3.09 |
| 600 | 5,954 | 2.12-2.34 | 2.54-2.71 | 2.70-4.12 |

All-v2 800 members: validate 2.23-2.40, update 1.86-1.94, merge 2.43-2.58.

At the shipped 500 the check failed once in five (merge 2.57 s), so the
member ceiling is 450 (links, child quotas and blobs 600). The check at 450,
load average about 6:

| state | signatures | validate | update | merge |
|---|---|---|---|---|
| ceilings | 5,242 | 1.36-1.46 s | 1.12-1.20 s | 1.47-1.54 s |
| legacy (mixed) | 4,642 | 1.65-1.82 s | 2.09-2.30 s | 2.13-2.40 s |
| official | 1,858 | 0.49-0.51 s | 0.41-0.44 s | 0.52-0.57 s |

`CHECK_SECS=2.5`: passed. The legacy content bounds (decision m) stay: the
mixed case has no room for larger legacy messages.

## 400 members, at most 300 legacy, 2026-10-27

The first build of the v1 codec (`fastcbor`) was slower than ciborium in
WASM, not faster: its decoder built an error `String` (`ok_or(Error(..))`,
evaluated eagerly) on every header and byte it read, so a legacy record
allocated and freed once per byte of its v1 bytes. With the errors built
lazily (`ok_or_else`) it is faster than ciborium. Same input, same host,
five runs each, minimum to maximum, load average 3.4 to 5.6:

| legacy (mixed, 400/300): 3,503 records, 4,211 signatures, 5.75 MB | validate | validate, no signatures | update | merge |
|---|---|---|---|---|
| ciborium (before the codec) | 1.51-1.70 s | 0.52-0.59 s | 1.98-2.50 s | 1.94-2.25 s |
| codec, eager errors | 1.71-1.86 s | 0.76-0.82 s | 2.54-2.76 s | 2.35-2.63 s |
| codec, lazy errors | 1.31-1.58 s | 0.32-0.51 s | 1.46-1.81 s | 1.58-1.91 s |

The other states, lazy-error codec:

| state | records | signatures | bytes | validate | update | merge |
|---|---|---|---|---|---|---|
| ceilings (all v2, 400 members) | 3,503 | 4,811 | 4.19 MB | 1.25-1.27 s | 1.05-1.13 s | 1.37-1.44 s |
| official | 1,435 | 1,857 | 0.87 MB | 0.45-0.47 s | 0.36-0.38 s | 0.46-0.48 s |
| live (Official room fetched 2026-10-26, converted) | 1,186 | 1,186 | 0.50 MB | 0.30-0.32 s | 0.31-0.36 s | 0.30-0.34 s |
| lmembers400:200 | 3,503 | 4,411 | 5.53 MB | 1.29-1.42 s | 1.46-1.57 s | 1.58-1.70 s |
| lmembers300:200 | 2,901 | 3,541 | 4.86 MB | 1.13-1.16 s | 1.29-1.37 s | 1.33-1.40 s |

`CHECK_SECS=2.5`: passed; the mixed legacy case keeps at least 0.6 s. A
legacy link now costs about what a v2 link does, so a legacy cap of 200
instead of 300 saves under 0.1 s.

Where the mixed case's time goes (perf over the guest's JIT code, lazy-error
codec): update 50% signature checks (curve25519 and SHA-512), 22% the v1
codec, 9% allocation, 7% BLAKE3, 5% memcmp/memcpy; merge 62% signatures, 17%
codec, 8% allocation, 4% BLAKE3.

### Tried and dropped: signing over the field's span

Checking a legacy signature over the signed field's span of the canonical
v1 bytes (instead of encoding the field again), with the canonical check
comparing as it encodes rather than building a buffer, gave no measurable
gain: interleaved runs on the mixed 400/300 case, medians of 15 calls,
validate 1.252 against 1.245 s, update 1.450 against 1.421 s, merge 1.523
against 1.529 s. The codec's cost is serde walking each v1 byte vector
element by element (decode, and the re-encode the canonical check needs),
not the second encode of the signed field or its allocation. The code is on
the branch `exp/room-state-v2-signed-span`.
