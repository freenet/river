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
The UI's `build.rs` generates Rust code from it at compile time. CI reads
it directly for validation.

## Single Source of Truth: `common/legacy_room_contracts.toml`

The room contract has its own registry, `common/legacy_room_contracts.toml`,
recording the BLAKE3 code hash of every previous room-contract WASM
generation. A client re-derives the contract key any owner's room used
under each generation and probes them newest-to-oldest to recover a room
dormant across one or more WASM upgrades (freenet/river#292).
`common/build.rs` generates `LEGACY_ROOM_CONTRACT_CODE_HASHES` from it;
the `river-core` `migration` feature exposes the lookup. It lives inside
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

# 3. Run migration tests
cargo test -p river-core --test migration_test
cargo test -p river-core --test room_contract_migration_test

# 4. Verify UI compiles with new generated code
cargo check -p river-ui --target wasm32-unknown-unknown --features no-sync

# 5. Commit everything
git add legacy_delegates.toml common/legacy_room_contracts.toml \
    ui/public/contracts/ cli/contracts/
git commit -m "fix: update WASMs with delegate migration entry"
```

## Validation

- **`cargo make check-migration`** — local check: builds delegate WASM and verifies migration entry exists if hash changed
- **`cargo test -p river-core --test migration_test`** — validates TOML entries: correct hex, 32-byte keys, delegate_key = BLAKE3(code_hash)
- **CI `check-delegate-migration` workflow** — builds base and PR WASMs, verifies old hash is in `legacy_delegates.toml`
- **CI `check-room-contract-migration` workflow** — verifies a changed room-contract WASM's old hash is in `common/legacy_room_contracts.toml`
- **CI `check-cli-wasm` workflow** — verifies `ui/public/contracts/` and `cli/contracts/` WASMs are in sync

## Key Files

| File | Purpose |
|------|---------|
| `legacy_delegates.toml` | Single source of truth for delegate migration entries |
| `common/legacy_room_contracts.toml` | Single source of truth for room-contract generations (#292) |
| `ui/build.rs` | Generates `LEGACY_DELEGATES` const from the delegate TOML |
| `common/build.rs` | Generates `LEGACY_ROOM_CONTRACT_CODE_HASHES` from the room-contract TOML |
| `common/src/migration.rs` | Re-derives legacy room-contract keys for backward recovery (#292) |
| `ui/src/components/app/chat_delegate.rs` | Uses generated `LEGACY_DELEGATES` for runtime migration |
| `scripts/check-migration.sh` / `scripts/add-migration.sh` | Delegate migration validation / entry |
| `scripts/check-room-contract-migration.sh` / `scripts/add-room-contract-migration.sh` | Room-contract registry validation / entry |
| `scripts/sync-wasm.sh` | Builds all WASMs and copies to committed locations |
| `common/tests/migration_test.rs` / `common/tests/room_contract_migration_test.rs` | Validate TOML entries are well-formed |

## Technical Details

- **Delegate key formula**: `BLAKE3(BLAKE3(wasm) || params)` — both steps use BLAKE3
- **DelegateKey equality** checks BOTH `key` AND `code_hash` fields
- **WASM on disk is versioned**: `store_delegate()` wraps raw WASM with `to_bytes_versioned()`. The code_hash in `.reg` files is authoritative.
- **WASM committed in 3 places**: `ui/public/contracts/`, `cli/contracts/`, and `target/` (build output). Use `cargo make sync-wasm` to keep them in sync.

See also `.claude/rules/river-publish.md` for the publish-side workflow
(including the runtime legacy-migration probe gate).
