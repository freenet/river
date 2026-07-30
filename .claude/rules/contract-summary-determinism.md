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

## Summary VALUES are a wire-format commitment too

Determinism is about the collection type; this section is about what goes in it.
A summary value that is only ever compared (never verified, never decoded back
into anything) should be a fixed-width digest, not the thing it fingerprints —
the summary is re-sent to every interested peer on every state change, so a
64-byte signature per entry is paid over and over.

When a summary carries a digest, four properties become wire format, and none of
them may change without re-keying the contract:

- which hash function (and it must be cryptographic if an attacker can choose
  the input — a base-31 polynomial like `freenet_scaffold::util::fast_hash` is
  fine for accidental collisions only);
- how wide, judged against who controls the colliding inputs. If a party can
  grind BOTH sides of the comparison, 64 bits is a ~2^32 birthday search, i.e.
  hours; use 128.
- which bytes are kept, and in what order;
- how the value serializes — a `[u8; 16]` through the serde derive emits a
  16-element CBOR array (32 bytes), not a byte string (17). Write `Serialize`
  by hand with `serialize_bytes`.

Pin all four with a **golden vector**: ONE fixed input, ONE fixed expected
digest, ONE fixed expected encoding. Oracles that compare digests of randomly
generated keys are NOT sufficient — a byte-order change leaves them agreeing
about half the time, so they detect it only intermittently (measured on this
codebase: 11 of 30 runs missed a reversed-byte-order change). See
`sig_digest_golden_vector` in `common/src/room_state/member_info.rs`.

Also assert bytes-per-entry for the summary, built by calling the real
`summarize()` with realistic key values — `MemberId(FastHash(i))` for small `i`
encodes in 1-3 bytes against a real key's ~9 and understates the entry by ~30%.
See `member_info_summary_stays_small_per_entry`.

## History

- **freenet/river** (2026-07): `MemberInfoV1::Summary` was
  `HashMap<MemberId, (u32, Signature)>`, `BansV1::Summary` and
  `MembersV1::Summary` were `HashSet`, `SecretsSummary` carried two `HashSet`s,
  and `DirectMessagesSummary.message_signatures` was a `HashSet` — all now
  `BTreeMap`/`BTreeSet`. `bincode` (the old wire path) doesn't care about key
  order, so this survived undetected until freenet-core added the
  summary-byte-compare staleness check.
- **freenet/river#571** (2026-07): the same summary's VALUE then shrank,
  `(u32, Signature)` → `(u32, SigDigest)`, where `SigDigest` is a 128-bit BLAKE3
  digest of the signature serialized as a CBOR byte string. The raw signature was
  66 of ~78 CBOR bytes per entry; measured 78 → 28.0 bytes/entry at 470 records.
  The collection type was already `BTreeMap` and did not change, so this is the
  value-side rule above rather than the determinism rule.
  `DirectMessagesSummary.message_signatures: BTreeSet<SignatureBytes>` still
  carries raw 64-byte signatures and has the same fix available.
- **freenet/freenet-core#4857** — the update-drop divergence this feeds.
