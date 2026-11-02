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

Results on 2026-11-02 at de0f73e16, WASM 34548ab9 (fdev 0.3.314, freenet-core
ecbe941ec, `--max-case-bytes 64000000` so no case is skipped for size,
`--max-states-paired 80`, 56 for the ceilings corpus, `--max-cases 3500`, 300
for the ceilings corpus). States sampled: official 45 of 45, universes 80 of
104, windowed 80 of 127, adversarial 25 of 25, ceilings 56 of 56. Transitions
sampled: at most 24. No removal-eligible property was violated on any corpus.
Every property held on every corpus except:

- `delta_permutation_invariance`, which is report-only and which a contract
  with caps cannot meet in its strict form (two deltas in one order can let a
  cap evict what the other order keeps): 158 of 2,160 cases on universes, 84
  of 3,280 on windowed, 126 of 300 on ceilings. Every such pair heals under
  further merges: `delta_permutation_pairs_heal` checks that over the
  universes corpus's histories,
  `delta_permutation_pairs_heal_in_the_windowed_corpus` over the windowed
  corpus's, `delta_permutation_pairs_heal_on_live_room` over a converted live
  room, and `delta_permutation_pairs_heal_at_the_ceilings` over the ceilings
  corpus's.
- The ceilings corpus at 300 cases: the case budget left out between 596 and
  1,492 cases of each pair and delta law, and 25 cases hit fdev's compute time
  limit, reported inconclusive (6 `state_commutativity`, 9
  `state_associativity`, 2 `delta_idempotence`, 5
  `delta_permutation_invariance`, 3 `reconciliation_cycle`). The machine was
  shared during this run, and an earlier rerun of the ceilings corpus with
  nothing else running had none.
- The universes and windowed corpora at 3,500 cases: the budget left out
  820-1,300 `delta_determinism` and 2,260-4,580 `delta_idempotence` cases.
- The adversarial corpus: its reordered, duplicated, over-cap and replayed
  states are refused by `validate_state`, so the verifier reports cases
  built on them as inconclusive ("input not valid"); it has no deltas or
  transitions, so those three laws have no cases.

The merge laws at the ceilings are also checked natively by the ignored
`the_laws_hold_at_the_ceilings` (every pair and triple of eight states).
