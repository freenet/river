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

Results on 2026-10-30 (fdev 0.3.311, freenet-core c86c91e1d): every property
held on every corpus except `delta_permutation_invariance`, which a contract
with caps cannot meet in its strict form (two deltas in one order can let a
cap evict what the other order keeps): 17 of 400 cases on the universes
corpus and 6 of 30 on the ceilings corpus. Every such pair heals under
further merges: `delta_permutation_pairs_heal` checks that in CI over random
universes, `delta_permutation_pairs_heal_on_live_room` over a converted live
room, and `delta_permutation_pairs_heal_at_the_ceilings` over the ceilings
corpus's histories (181 of 336 pairs differ, all heal). Calls on the largest
states can exceed fdev's compute time limit on a loaded machine; the cases
are then inconclusive ("resource limit hit"), and hold when run alone. Adversarial states (reordered, duplicated, over-cap, replayed
superseded records) are refused by `validate_state`, so the verifier reports
cases built on them as inconclusive ("input not valid").
