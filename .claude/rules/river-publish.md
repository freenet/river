---
description: When publishing River to Freenet, deploying updates, or discussing the publish/deploy workflow
globs: []
---

# River Publish Workflow

Builds and deploys River to Freenet, handling delegate/contract key migration to prevent user data loss.

## Key Principle

**Delegate keys = BLAKE3(BLAKE3(WASM) || params).** Any change to the delegate WASM — code changes, dependency updates, even transitive dependency changes — produces a new delegate key. Old secrets (room data, signing keys) are stored under the old key and become inaccessible. You MUST add migration entries before deploying.

**CRITICAL: CodeHash = BLAKE3(wasm), NOT SHA256(wasm).** A Feb 2026 incident produced wrong migration keys because SHA256 was used instead of BLAKE3, breaking delegate migration and losing user rooms.

## Pre-Publish Checklist

### Step 1: Determine if Delegate/Contract WASM Changed

```bash
git diff HEAD~1 --name-only | grep -E '(common/|delegates/|contracts/|Cargo\.(toml|lock))'
```

If ANY match, migration is needed. If ONLY `ui/` changed, skip to Step 4.

### Step 2: Add Migration Entry

```bash
cargo make add-migration  # Run BEFORE your changes alter the WASM
```

If changes are already in the tree, stash first: `git stash`, run add-migration, `git stash pop`.

### Step 3: Build New WASMs and Sync

```bash
cargo make sync-wasm
```

### Step 4: Validate and Test

```bash
cargo make check-migration
cargo test -p river-core --test migration_test
cargo check -p river-ui --target wasm32-unknown-unknown --features no-sync
cargo fmt
```

### Step 5: Commit, Build, and Publish

```bash
git add legacy_delegates.toml ui/public/contracts/ cli/contracts/
git commit -m "fix: <description> with delegate migration"
cargo make build
cargo make compress-webapp
cargo make publish-river  # If version error, see AGENTS.md "Manual publish"
```

### Step 6: Verify and Push

```bash
curl -s http://127.0.0.1:7509/v1/contract/web/raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv/ | head -5
git push origin main
```

## Publishing River UI + riverctl Together

**CRITICAL:** When room contract WASM changes, both UI and riverctl must be republished — they embed the WASM and derive the contract key from it.

```bash
cargo make publish-all
```

## How Delegate Migration Works

1. On startup, `set_up_chat_delegate()` fires `fire_legacy_migration_request()`
2. `LEGACY_DELEGATES` is generated at compile time from `legacy_delegates.toml` by `ui/build.rs`
3. Sends `GetRequest` for `rooms_data` to EACH legacy key
4. Response handler migrates room data + signing keys to the current delegate
5. `mark_legacy_migration_done()` sets localStorage flag to skip on next load

## Migration Limitations

- Old delegate WASM must be in the node's delegate store (registered in a previous session)
- stdlib versions before ~0.1.34 used removed enum variants — old WASM can't execute on new runtime
- Host function API must be compatible between old and new WASM

## Common Mistakes

- **Wrong hash algorithm**: BLAKE3 not SHA256 for CodeHash
- **Forgetting migration**: Users lose all room data
- **Computing key AFTER changes**: Must run `add-migration` BEFORE changes alter the WASM
- **Not republishing riverctl**: Use `cargo make publish-all` when WASM changes
- **Parameters file**: Always use `published-contract/webapp.parameters` (committed) — determines contract ID

## Contract ID

`raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv`
