# Contract `Summary` / `Delta` Determinism

**Every field in a contract's `ComposableState::Summary` (and any `Delta` whose
bytes are compared) MUST serialize deterministically. Use `BTreeMap`/`BTreeSet`
or a deterministically-sorted `Vec` — NEVER `HashMap`/`HashSet` or an
unsorted `Vec` whose order isn't stable across peers.**

## Why this is load-bearing

freenet-core decides whether a peer is stale by **byte-comparing** the output of
the contract's `summarize_state` (the `is_stale` check). Two peers holding the
**identical** logical state must produce **byte-identical** summary bytes, or the
"summaries are equal → skip" fast path never fires.

A `HashMap`/`HashSet` iterates in a **per-process-random order** (its
`RandomState` seed differs per map instance / per process). So two peers with the
same state serialize their summary in different orders → different bytes →
freenet-core thinks they are perpetually out of sync → the anti-entropy
heartbeat fires a spurious **full-state heal for every room on every cycle**.

This was observed in production as ~20M `summarize_contract_state` calls, and it
feeds the update-drop divergence in **freenet/freenet-core#4857**.

The same applies to an unsorted `Vec`: if the `Vec`'s element order is not the
same on every peer for the same logical contents, its serialization differs.
(A `Vec` that is kept in a canonical sorted order — e.g. `MessagesV1` keeps
`messages` sorted by `(time, id)` in `apply_delta` — is fine.)

## The rule

For any `impl ComposableState` in a contract (River's live in `common/src/room_state/`):

- `type Summary` and every field of a struct/enum used as `Summary`:
  - `HashMap<K, V>` → `BTreeMap<K, V>` (K must be `Ord`)
  - `HashSet<T>` → `BTreeSet<T>` (T must be `Ord`)
  - unsorted `Vec<T>` → sort it by a stable key before returning, OR keep the
    underlying state `Vec` in a canonical order and document that.
- Prefer changing ONLY the summary/delta collection type. Do NOT change the
  STATE type — `validate_state` must still accept existing stored state
  byte-for-byte (only `summarize_state` output changes).
- Add a **determinism test** for each summary type: build the same logical
  summary with elements inserted in two different orders, serialize with
  `ciborium::ser::into_writer` (exactly what `summarize_state` uses), and assert
  the bytes are byte-identical. Reference the associated type
  (`<T as ComposableState>::Summary`) or the real struct field so the test FAILS
  if someone reverts to `HashMap`/`HashSet`. See
  `common/tests/summary_determinism_test.rs`.

## This is a WASM change → migration

Changing a summary collection type changes the contract WASM → the contract key
changes → follow the room-contract + delegate migration ritual
(`.claude/rules/delegate-migration.md`) before publishing, and bump the
`river-core` / `riverctl` versions if a WASM changed.

## History

- **freenet/river** (2026-07): `MemberInfoV1::Summary` was
  `HashMap<MemberId, (u32, Signature)>`, `BansV1::Summary` and
  `MembersV1::Summary` were `HashSet`, `SecretsSummary` carried two `HashSet`s,
  and `DirectMessagesSummary.message_signatures` was a `HashSet` — all now
  `BTreeMap`/`BTreeSet`. `bincode` (the old wire path) doesn't care about key
  order, so this survived undetected until freenet-core added the
  summary-byte-compare staleness check.
- **freenet/freenet-core#4857** — the update-drop divergence this feeds.
