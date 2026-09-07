---
description: When modifying code in common/, delegates/, contracts/, or updating Cargo.toml/Cargo.lock — any change that could alter delegate or contract WASM
globs:
  - common/**
  - delegates/**
  - contracts/**
  - Cargo.toml
  - Cargo.lock
---

# Delegate & Contract WASM Migration Required

When delegate or contract WASM changes (due to code changes in `delegates/`,
`contracts/`, or `common/`), the delegate/contract key changes. Without a
migration entry, **users lose all room data**.

## Quick Reference

1. `cargo make add-migration` — computes old delegate key and appends to `legacy_delegates.toml`
2. `cargo make sync-wasm` — builds new WASMs and copies to all committed locations
3. `cargo make check-migration` — validates the migration entry exists
4. `cargo test -p river-core --test migration_test` — validates TOML entries are well-formed

**Key rules:**
- Run `add-migration` BEFORE your changes alter the WASM (stash changes first if needed)
- **Single source of truth**: `legacy_delegates.toml` — never manually edit byte arrays
- **Both steps use BLAKE3**: `code_hash = BLAKE3(wasm)`, `delegate_key = BLAKE3(code_hash)` — NOT SHA256
- **Publish both UI and riverctl** when WASM changes: `cargo make publish-all`

## Single Source of Truth: `legacy_delegates.toml`

All legacy delegate entries are defined in `legacy_delegates.toml` at the
repo root. This file is the **only** place migration entries are managed.
`ui/build.rs` turns it into Rust at compile time by calling
`freenet_migrate_build::codegen()`; the build script itself keeps only the
local gate that fails the build when the generated table comes out empty.
CI reads the TOML directly for validation.

## Single Source of Truth: `common/legacy_room_contracts.toml`

The room contract has its own registry, `common/legacy_room_contracts.toml`,
recording the BLAKE3 code hash of every previous room-contract WASM
generation. A client re-derives the contract key any owner's room used
under each generation and probes them newest-to-oldest to recover a room
dormant across one or more WASM upgrades (freenet/river#292).
`common/build.rs` generates `LEGACY_ROOM_CONTRACT_CODE_HASHES` from it via
the same `freenet_migrate_build::codegen()` call; the `river-core`
`migration` feature exposes the lookup. It lives inside
the `common` crate (not the repo root) so it ships with the published
`river-core` crate and riverctl keeps the full registry.

## Upgrade Workflow

```bash
# 1. BEFORE rebuilding any WASM, record the OLD (currently-committed) hashes.
#    Both scripts hash the WASM as it sits on disk now, so they must run
#    before step 2 rebuilds it. If your changes already rebuilt the WASM,
#    `git checkout HEAD -- ui/public/contracts/ cli/contracts/` first.
cargo make add-migration
#    AND, if the room-contract WASM changed, add its old hash too:
cargo make add-room-contract-migration

# 2. Build new WASMs and copy to all committed locations
cargo make sync-wasm

# 2b. Re-sign the pointer records against the WASM you just built.
#     Step 1 carries OUR users forward; this carries THIRD PARTIES forward
#     (see "The other half of a re-key" below). Same trigger, and CI's
#     check-pointer-freshness fails the PR if you skip it.
cargo make sign-pointer-records

# 3. Run migration tests
cargo test -p river-core --test migration_test
cargo test -p river-core --test room_contract_migration_test
cargo make check-pointer-freshness

# 4. Verify UI compiles with new generated code
cargo check -p river-ui --target wasm32-unknown-unknown --features no-sync

# 5. Commit everything
git add legacy_delegates.toml common/legacy_room_contracts.toml \
    pointer-records.toml ui/public/contracts/ cli/contracts/
git commit -m "fix: update WASMs with delegate migration entry"

# 6. AFTER the PR merges, from main: publish the re-signed records.
#    Signing is offline and belongs in the PR; the network write does not.
#    See .claude/rules/river-publish.md Step 7.
```

## The other half of a re-key: pointer records

The registries above carry **our own users'** data across a re-key. They do
nothing for a **third party** integrating with River, whose reference to our
contract or delegate key is a build-time constant that silently goes stale —
every read comes back looking like "this user has nothing stored".

That half is `pointer-records.toml`: a record at a FIXED address naming the
artifact's current code hash, signed by River's author key, which integrators
resolve instead of pinning. Same trigger as a migration entry, different
beneficiary.

```bash
cargo make sign-pointer-records      # after sync-wasm, BEFORE committing
cargo make check-pointer-freshness   # what CI runs
# then, from main after the PR merges:
cargo make publish-pointer-records
```

CI's `check-pointer-freshness` fails the PR if a pointed-at WASM changed and no
new record was signed. So in practice: whenever you run `add-migration`, you
also need `sign-pointer-records`. See FREENET.md for the integrator-facing side,
including the scope boundary — **a pointer solves addressing only** and says
nothing about whether secrets survived the re-key, which is exactly what the
rest of this file is about.

## Validation

- **`cargo make check-migration`** — local check: builds delegate WASM and verifies migration entry exists if hash changed
- **`cargo test -p river-core --test migration_test`** — validates TOML entries: correct hex, 32-byte keys, delegate_key = BLAKE3(code_hash)
- **CI `check-delegate-migration` workflow** — builds base and PR WASMs, verifies old hash is in `legacy_delegates.toml`
- **CI `check-room-contract-migration` workflow** — verifies a changed room-contract WASM's old hash is in `common/legacy_room_contracts.toml`
- **CI `check-cli-wasm` workflow** — verifies `ui/public/contracts/` and `cli/contracts/` WASMs are in sync
- **CI `check-pointer-freshness` workflow** — verifies every record in `pointer-records.toml` still names the committed WASM and is signed by the author key published in `FREENET.md`

## Key Files

| File | Purpose |
|------|---------|
| `legacy_delegates.toml` | Single source of truth for delegate migration entries |
| `common/legacy_room_contracts.toml` | Single source of truth for room-contract generations (#292) |
| `ui/build.rs` | Calls `freenet_migrate_build::codegen()` to generate the `LEGACY_DELEGATES` const from the delegate TOML |
| `common/build.rs` | Calls `freenet_migrate_build::codegen()` to generate `LEGACY_ROOM_CONTRACT_CODE_HASHES` from the room-contract TOML |
| `common/src/migration.rs` | Re-derives legacy room-contract keys for backward recovery (#292) |
| `ui/src/components/app/chat_delegate.rs` | Uses generated `LEGACY_DELEGATES` for runtime migration |
| `scripts/check-migration.sh` / `scripts/add-migration.sh` | Delegate migration validation / entry |
| `scripts/check-room-contract-migration.sh` / `scripts/add-room-contract-migration.sh` | Room-contract registry validation / entry |
| `pointer-records.toml` | Signed pointer records — third-party stable identity (addressing only) |
| `scripts/check-pointer-freshness.sh` / `scripts/sign-pointer-records.sh` / `scripts/publish-pointer-records.sh` | Pointer gate / re-sign / network publish |
| `scripts/sync-wasm.sh` | Builds all WASMs and copies to committed locations |
| `common/tests/migration_test.rs` / `common/tests/room_contract_migration_test.rs` | Validate TOML entries are well-formed |

## Comment-only edits re-key the room contract

**Measured, 2026-09-06.** Same canonical co-build, same absolute path, a single
`// probe` comment line at the top of `direct_messages.rs` as the only variable:

```
unmodified                       room_contract.wasm = 0ab72165…
+ one comment line               room_contract.wasm = 237a17ba…
```

The contract WASM embeds panic `Location` records, which carry file **and
line** — `strings` on the artifact shows `common/src/room_state/direct_messages.rs`,
`configuration.rs`, `member_info.rs` and `util.rs`. Inserting or deleting a
comment line in any of those shifts every `panic!`/`expect`/`assert` site below
it and changes the code hash, hence the contract key. The chat delegate is
usually unaffected because dead-code elimination drops those paths from it.

So **"it's only a comment" is not a reason to skip the migration ritual.** If
you edit one of those files at all, either rebuild and record the entry, or keep
the edit LINE-NEUTRAL (each replacement block the same line count as the text it
replaces) and prove it by rebuilding and comparing hashes.

Two related things worth keeping straight:

- **`check-wasm-sync` will NOT catch a source/artifact mismatch.** It compares
  `ui/public/contracts/` against `cli/contracts/` — the two committed copies —
  not against a build of the source. A docs-only push without a rebuild ships an
  artifact that is not the build of its own tree, and every gate stays green.
- **Registry edits ARE hash-neutral**, and that is a narrower result than it
  looks. Appending to `common/legacy_room_contracts.toml` leaves both WASMs
  byte-identical because the generated table sits behind
  `#[cfg(feature = "migration")]` (`common/src/lib.rs`), which the contract build
  does not enable — not because non-code changes are free. Do not generalise it.

## Technical Details

- **Delegate key formula**: `BLAKE3(BLAKE3(wasm) || params)` — both steps use BLAKE3
- **DelegateKey equality** checks BOTH `key` AND `code_hash` fields
- **WASM on disk is versioned**: `store_delegate()` wraps raw WASM with `to_bytes_versioned()`. The code_hash in `.reg` files is authoritative.
- **WASM committed in 3 places**: `ui/public/contracts/`, `cli/contracts/`, and `target/` (build output). Use `cargo make sync-wasm` to keep them in sync.

See also `.claude/rules/river-publish.md` for the publish-side workflow
(including the runtime legacy-migration probe gate).
