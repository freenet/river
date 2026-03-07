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

Changes to these paths can alter the delegate or contract WASM hash, which changes the delegate/contract key. Without a migration entry, **users lose all room data**.

## Before publishing, you MUST:

1. `cargo make add-migration` — computes old delegate key and appends to `legacy_delegates.toml`
2. `cargo make sync-wasm` — builds new WASMs and copies to all committed locations
3. `cargo make check-migration` — validates the migration entry exists
4. `cargo test -p river-core --test migration_test` — validates TOML entries are well-formed

## Key rules:

- **Run `add-migration` BEFORE your changes alter the WASM** (stash changes first if needed)
- **Single source of truth**: `legacy_delegates.toml` — never manually edit byte arrays
- **Both steps use BLAKE3**: `code_hash = BLAKE3(wasm)`, `delegate_key = BLAKE3(code_hash)` — NOT SHA256
- **Publish both UI and riverctl** when WASM changes: `cargo make publish-all`

See AGENTS.md "Delegate & Contract WASM Migration" for full details.
