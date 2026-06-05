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

**Exception — feature-gated client-only `common/` modules.** A change confined to
a `common/` module gated behind a client-only Cargo feature (one enabled by
`river-ui`/`riverctl` but NOT by the room-contract or chat-delegate crates —
e.g. `migration`, `mentions`) does **not** enter the delegate/contract WASM, so
it needs no migration entry. This holds only because those WASMs are built with
`-p <crate>` scoping (never `--workspace`, which would unify the feature in).
Don't add a migration entry for such a change — CI's `check-delegate-migration`
and `check-room-contract-migration` confirm the WASM is byte-identical and will
fail loudly if it isn't.

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

Rooms are stored as **per-room keys** since freenet/river#345: each room is its
own `room:<base58(owner_vk)>` delegate key holding a `RoomSlot::{Present|Tombstone}`,
plus a single `rooms_meta` key for list-level view prefs. The legacy single
`rooms_data` blob is **no longer written** (still read once for migration). Each
key is independently CAS-versioned (`GetVersionedRequest`/`CasStoreRequest`).

Legacy migration is **gated on the current delegate's state** to avoid a race
where stale legacy data overwrites newer state on the current delegate
(freenet/river#253). The flow:

1. On startup, `set_up_chat_delegate()` fires `fire_list_rooms_request()` (a
   `ListRequest`) AND `fire_load_outbound_dms_request()` for the **current**
   delegate only. It does NOT fire the legacy probes. (The startup load is now
   List-driven, not a `GetRequest{rooms_data}`, because the per-room keys are
   dynamic and must be discovered.)
2. `LEGACY_DELEGATES` is generated at compile time from `legacy_delegates.toml`
   by `ui/build.rs`.
3. The current delegate's `ListResponse` is classified by `plan_load_from_keys`:
   - **Has `room:<vk>` keys** → current is authoritative: `load_rooms_per_room`
     plain-GETs each slot (+ `rooms_meta`), reconstructs `Rooms`, hydrates, and
     calls `mark_legacy_migration_done()`. Legacy probes never fire.
   - **No per-room keys, only a legacy `rooms_data` blob** →
     `migrate_current_blob_to_per_room` reads it and re-saves as per-room keys
     (non-destructive — the blob is left as a rollback fallback).
   - **Nothing** → `fire_legacy_migration_request()`.
4. `fire_legacy_migration_request` probes each legacy delegate TWO ways:
   (a) fixed `GetRequest` for `[ROOMS_STORAGE_KEY, OUTBOUND_DMS_STORAGE_KEY]`
   (the pre-#345 single-blob format + DM cache), and (b) a `ListRequest` to
   discover the legacy delegate's **dynamic per-room keys** — without (b), the
   first delegate-WASM bump AFTER per-room storage shipped would strand every
   room under the now-legacy per-room key. A legacy `ListResponse` routes to
   `migrate_legacy_per_room`, which GETs those slots from the legacy delegate and
   re-saves them to the current one.
5. The response handler migrates room data + signing keys from each responding
   legacy delegate to the current delegate. The outbound-DM-plaintext blob
   (`OUTBOUND_DMS_STORAGE_KEY`, issue freenet/river#256) goes through
   `handle_outbound_dms_get_response` in `response_handler.rs`, which merges into
   the in-memory `OUTBOUND_DMS` signal — keeping whichever entry has the larger
   `timestamp` on collision so a later-arriving legacy response can't clobber a
   fresher current entry.
6. On a legacy delegate returning data, `mark_legacy_migration_done()` is called
   after the post-merge save succeeds; on a legacy delegate returning no data it
   is called immediately.

**Trade-off**: a user with data in both current and legacy delegates will
NOT have the legacy `rooms_data` merged. This is intentional — the
alternative race destroys newer data, which is much worse than failing to
recover older data the user has effectively abandoned. The same applies
to legacy `outbound_dms`: it is only probed when the gate above fires,
i.e. when the current delegate's `rooms_data` is empty. A user with
rooms on the current delegate but outbound DM plaintext only on a legacy
delegate will not have those plaintexts migrated.

**Adding new storage keys**: how to make a new top-level key survive a delegate
rebuild depends on the key's shape:
- **Fixed, single-key** (like `outbound_dms`): add it to the `storage_keys`
  array in `fire_legacy_migration_request` AND route its `GetResponse` in
  `response_handler.rs` next to the existing `OUTBOUND_DMS_STORAGE_KEY` arm.
- **Dynamic / open-ended key families** (like `room:<vk>`): a fixed probe can't
  enumerate them. They are discovered via the legacy `ListRequest` →
  `migrate_legacy_per_room` path instead. A new dynamic family needs its own
  branch there (and in the current-delegate `load_rooms_per_room`).

Missing the relevant side leaves the new key orphaned across delegate rebuilds —
exactly the bug the per-room `ListRequest` legacy probe (freenet/river#345) was
added to prevent.

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
