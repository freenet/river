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
cargo make publish-river  # bumps published-contract/contract-version.txt
```

### Step 6: Commit the version bump, verify, and push

```bash
# `cargo make sign-webapp` (run transitively by publish-river) incremented
# the counter. Commit it together with whatever other changes the publish
# included.
git add published-contract/contract-version.txt
git commit -m "chore: bump web-container version after publish"
curl -s http://127.0.0.1:7509/v1/contract/web/raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv/ | head -5
git push origin main
```

**Version policy:** the canonical source is `published-contract/contract-version.txt`, which `cargo make sign-webapp` increments and writes back each publish. NEVER base the version on wall-clock time (`date +%s / 60`) — the previous scheme bit us 2026-05-16 when the on-network version had drifted ahead of the timestamp-derived value. The counter file makes the version a strict monotonic local invariant; gaps are fine (the contract enforces monotonicity, not contiguity).

## Publishing River UI + riverctl Together

**CRITICAL:** When room contract WASM changes, both UI and riverctl must be republished — they embed the WASM and derive the contract key from it.

```bash
cargo make publish-all
```

## How Delegate Migration Works

Legacy migration is **gated on the current delegate's response** to avoid a
race where stale legacy data overwrites newer state on the current delegate
(freenet/river#253). The flow:

1. On startup, `set_up_chat_delegate()` fires `fire_load_rooms_request()` for
   the **current** delegate only. It does NOT fire the legacy probes.
2. `LEGACY_DELEGATES` is generated at compile time from `legacy_delegates.toml`
   by `ui/build.rs`.
3. When the current delegate's `GetResponse` for `rooms_data` arrives:
   - **Has authoritative data** (rooms or tombstones): call
     `mark_legacy_migration_done()` (persists across sessions). Legacy probes
     are never fired — current is the source of truth.
   - **Empty or missing**: call `fire_legacy_migration_request()` to probe
     each legacy key with `GetRequest` for `rooms_data`. Safe because current
     has nothing to clobber.
4. The response handler migrates room data + signing keys from each
   responding legacy delegate to the current delegate.
5. On a legacy delegate returning data, `mark_legacy_migration_done()` is
   called after the post-merge save succeeds; on a legacy delegate returning
   no data it is called immediately.

**Trade-off**: a user with data in both current and legacy delegates will
NOT have the legacy data merged. This is intentional — the alternative
race destroys newer data, which is much worse than failing to recover
older data the user has effectively abandoned.

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
