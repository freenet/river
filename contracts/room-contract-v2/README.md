# room-contract-v2

River's room state v2 (`river_core::room_state_v2`) behind the contract
interface. A new contract with its own key; the deployed room contract is
`../room-contract`. Not published.

```bash
# From the river repo root (the pinned toolchain):
cargo build --manifest-path contracts/room-contract-v2/Cargo.toml \
  --release --target wasm32-unknown-unknown --target-dir <dir>
```

## Checking it with freenet-core's merge verifier

`river-core`'s ignored test `write_fdev_corpora` writes state corpora and
replay bundles (with deltas, summaries and transitions); then, per corpus and
property:

```bash
RIVER_V2_CORPUS=<out> RIVER_V2_WASM=<dir>/wasm32-unknown-unknown/release/room_contract_v2.wasm \
  RIVER_V1_STATE=<a fetched v1 state> RIVER_V1_OWNER=<owner key, base58> \
  cargo test -p river-core --lib write_fdev_corpora -- --ignored
fdev verify-merge --bundle <out>/official.bundle --wasm <wasm> --property state_associativity --json
```

Results on 2026-10-30 (fdev 0.3.313, freenet-core fe2341c59, `--max-case-bytes
64000000` so no case is skipped for size, `--max-cases 3000`, 300 for the
ceilings corpus). fdev samples at most 24 of a corpus's states and 24 of its
transitions: official 24 of 45 states, universes 24 of 104, windowed 24 of
127, adversarial 24 of 25, ceilings 24 of 56. Every property held on every
corpus except:

- `delta_permutation_invariance`, which a contract with caps cannot meet in
  its strict form (two deltas in one order can let a cap evict what the
  other order keeps): 43 of 648 cases on universes, 32 of 984 on windowed,
  138 of 300 on ceilings. Every such pair heals under further merges:
  `delta_permutation_pairs_heal` checks that over the universes corpus's
  histories, `delta_permutation_pairs_heal_in_the_windowed_corpus` over the
  windowed corpus's, `delta_permutation_pairs_heal_on_live_room` over a
  converted live room, and `delta_permutation_pairs_heal_at_the_ceilings`
  over the ceilings corpus's (242 of 336 pairs differ, all heal).
- The ceilings corpus at 300 cases: the case budget left out 108
  `delta_determinism`, 468 `delta_idempotence` and 84
  `delta_permutation_invariance` cases, and 22 cases hit fdev's compute time
  limit on a loaded machine (6 `delta_determinism`, 15
  `reconciliation_cycle`, 1 `path_agreement`), reported inconclusive.
- The adversarial corpus: its reordered, duplicated, over-cap and replayed
  states are refused by `validate_state`, so the verifier reports cases
  built on them as inconclusive ("input not valid"); it has no deltas or
  transitions.

The merge laws at the ceilings are also checked natively by the ignored
`the_laws_hold_at_the_ceilings` (every pair and triple of eight states).
