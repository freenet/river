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

### Step 3b: Re-sign the pointer records

The WASM you just rebuilt has a new code hash, so River's **pointer records**
now name a key that no longer exists. Those records are what third parties
resolve instead of pinning a key — see `pointer-records.toml` and FREENET.md.

```bash
cargo make sign-pointer-records   # re-signs against the WASM you just built
```

Step 2's migration entry carries **our own users'** data forward. This carries
**everyone else** forward. Same trigger, different set of people who lose if it
is skipped — and their failure is quieter, because a stale pointer answers
confidently with a dead key rather than erroring.

CI's `check-pointer-freshness` fails the PR if you skip this, so it is not
possible to publish a stale pointer through the normal flow. That is the
backstop, not the plan.

### Step 4: Validate and Test

```bash
cargo make check-migration
cargo make check-pointer-freshness
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
# included — and commit it EVEN IF the publish reported a failure. See
# "Version policy" below: the counter is forward-only, because a reported
# failure is not evidence the state did not land.
git add published-contract/contract-version.txt
git commit -m "chore: bump web-container version after publish"
curl -s http://127.0.0.1:7509/v1/contract/web/raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv/ | head -5
git push origin main
```

### Step 7: Publish the pointer records (only if step 3b re-signed any)

Signing (3b) happens in the PR; publishing is the network half and runs from
`main` after the merge, like every other publish here.

```bash
POINTER_NODE_PORT=<a network node of yours, NOT 7509> \
POINTER_WASM=~/code/freenet/freenet-migrate/contracts/pointer-contract/pointer-v1.wasm \
  cargo make publish-pointer-records
```

It runs its own pre-flight (clean tree, on main, CI green on this exact SHA,
the record's hash matches the artifact **live on the network**, the version is
exactly one above what the network holds) and then re-reads each record back
and verifies it. Do not accept "the PUT returned OK": a publish at an
already-used version is a no-op *success*, so the network will never tell you
it was ignored.

**Version policy:** the canonical source is `published-contract/contract-version.txt`, which `cargo make sign-webapp` increments and writes back each publish. NEVER base the version on wall-clock time (`date +%s / 60`) — the previous scheme bit us 2026-05-16 when the on-network version had drifted ahead of the timestamp-derived value. The counter file makes the version a strict monotonic local invariant; gaps are fine (the contract enforces monotonicity, not contiguity).

**The counter is forward-only. Never re-use a version, and never hand-edit it downwards.** A publish that reports a failure is not evidence the state did not land — on 2026-08-04 `fdev` reported `put timed out after 1 peer attempt(s)` while the state was on the network the whole time. The old flow rolled the counter back on that exit code, the operator retried, the rebuild was not byte-reproducible, and a *different* archive got signed at the *same* version. Two states at one version never converge (the contract rejects `version <= current` in both directions, and the state summary carries only the version, so anti-entropy cannot tell them apart), and the only recovery is the next publish at a higher version. Burning a version costs a gap in the sequence and nothing else.

So: **commit the bumped counter whether or not the publish reported success.** `publish-river` then reads the state back and prints an advisory report — the live version, and whether its archive is byte-for-byte the one you just published. Read it before deciding anything. It is a report, not a gate: it cannot fail a publish, and `NOT SEEN YET` means "one GET did not find it yet", which on an eventually-consistent network is not the same as "it did not land". Re-read with `fdev network execute get <contract_id> -o /tmp/state.bin` rather than republishing on a single early read.

## Two independent release surfaces

The river repo publishes **two things on independent cadences** — do not conflate them:

| Artifact | Where it goes | How it's released |
|----------|---------------|-------------------|
| **riverctl** (the CLI) | crates.io (`river-core` + `riverctl`) + GitHub release with prebuilt binaries | **tag-triggered CI** — see below |
| **River UI** (Dioxus webapp) | Freenet (contract key `raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv`) | `cargo make publish-river` (the checklist above) |

They are NOT 1:1 — a CLI-only fix ships riverctl without touching the UI, and a UI-only change publishes to Freenet without a new riverctl. The one coupling is room-contract WASM: when it changes, BOTH must be republished (they embed the WASM and derive the contract key from it).

## Releasing riverctl (crates.io + GitHub release) — tag-triggered

**Canonical path since 2026-07.** riverctl is released by pushing a git tag; the
`.github/workflows/release-riverctl.yml` workflow then publishes to crates.io
AND cuts the GitHub release (with prebuilt Linux/macOS/Windows binaries and
auto-generated notes). This is why the GitHub releases page previously rotted at
v0.1.22 while crates.io marched on — publishing used to be a laptop-only
`cargo publish` that never touched GitHub.

```bash
# 1. Bump the version in cli/Cargo.toml (via a normal PR), and — if the
#    common/ crate (river-core) also changed — bump [workspace.package] version
#    and riverctl's `river-core = { version = "..." }` requirement to match.
#    Merge to main. (CI's check-cli-wasm guards the embedded room_contract.wasm.)

# 2. From main, tag with the riverctl version and push the tag:
git checkout main && git pull
git tag riverctl-v$(grep -m1 '^version' cli/Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
git push origin riverctl-v<version>
```

The workflow then:
1. **verify** — refuses to run unless the tag version == `cli/Cargo.toml` version
   AND the tagged commit is on `origin/main` (publish-from-main gate).
2. **publish-crates** — publishes `river-core` (only if that version isn't already
   on crates.io, waiting for index propagation) then `riverctl`. Idempotent, so a
   CLI-only release skips the unchanged river-core.
3. **build-binaries** — cross-builds `riverctl` for x86_64 Linux, x86_64 + aarch64
   macOS, and x86_64 Windows.
4. **github-release** — creates the GitHub release with those binaries + notes.

**Requires repo secret `CARGO_REGISTRY_TOKEN`** (a crates.io publish-update token
scoped to `river-core` + `riverctl`):
```bash
gh secret set CARGO_REGISTRY_TOKEN --repo freenet/river
```

**Tag scheme:** `riverctl-v<version>` (the `riverctl-` prefix disambiguates from
the historical bare `v*` tags and leaves room for a future `ui-v*` scheme). The
version is riverctl's own (`cli/Cargo.toml`), independent of `river-core`'s
workspace version.

### Local `cargo make publish-all` (legacy / fallback)

`cargo make publish-all` still exists and publishes UI + bumps + `cargo publish`
riverctl from your laptop. Prefer the tag-triggered CI path above for riverctl —
it publishes from main under review and keeps crates.io and GitHub in sync. Use
`publish-all` only for the coupled UI+riverctl WASM-change case when you
specifically need to drive the Freenet publish and the CLI publish together
locally; even then, consider publishing the UI locally and releasing riverctl via
the tag.

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
   by `ui/build.rs`, which delegates the codegen itself to
   `freenet_migrate_build::codegen()`.
3. The current delegate's `ListResponse` is classified by `plan_load_from_keys`:
   - **Has `room:<vk>` keys** → `load_rooms_per_room` plain-GETs each slot
     (+ `rooms_meta`), reconstructs `Rooms`, and hydrates. What it does about
     legacy migration is gated by `decide_per_room_load_action` on the
     **interrupted-migration flag** (`is_legacy_migration_in_progress()`):
     - *Not interrupted* (the common case) → the per-room set is authoritative:
       call `mark_legacy_migration_done()`; legacy probes never fire (#253).
     - *Interrupted* (a prior legacy→per-room migration was cut short before it
       wrote every key — freenet/river#345 follow-up, Nacho's "Freenet Devs"
       disappeared-after-update) → do NOT mark done; after loading the partial
       set, re-run `migrate_current_blob_to_per_room()` once (armed via
       `arm_legacy_migration_recovery()`, which also clears the per-session
       attempt guard so same-session recovery isn't a no-op) to recover any
       stranded room. The re-save is per-room CAS read-merge-write, so an
       already-present room merges rather than being clobbered.
   - **No per-room keys, only a legacy `rooms_data` blob** →
     `migrate_current_blob_to_per_room` reads it and re-saves as per-room keys
     (non-destructive — the blob is left as a rollback fallback).
   - **Nothing** → `fire_legacy_migration_request()`.

   The **interrupted-migration flag** is a per-legacy-set localStorage marker
   (`river_legacy_migration_in_progress:<fingerprint>`, parallel to the
   `…_done:` flag): set BEFORE any migration re-save (`hydrate_loaded_rooms`'s
   legacy branch and the current-blob explosion alike) and cleared ONLY on a
   FULL successful re-save. A partial/aborted re-save therefore leaves it set,
   which is what drives the recovery above.
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
rebuild depends on the key's shape. Note there are now **three** sites, not two —
the crate walk (below) carries its own fixed-probe list:
- **Fixed, single-key** (like `outbound_dms`): add it to the `storage_keys`
  array in `fire_legacy_migration_request`, route its `GetResponse` in
  `response_handler.rs` next to the existing `OUTBOUND_DMS_STORAGE_KEY` arm,
  AND add it to the crate walk's fixed-probe list in
  `ui/src/components/app/freenet_api/delegate_migration.rs` (the
  `for fixed in [ROOMS_STORAGE_KEY, OUTBOUND_DMS_STORAGE_KEY]` loop, which is
  asked even when unlisted because a frozen legacy WASM reports a corrupt index
  as empty).
- **Dynamic / open-ended key families** (like `room:<vk>`): a fixed probe can't
  enumerate them. They are discovered via the legacy `ListRequest` →
  `migrate_legacy_per_room` path instead. A new dynamic family needs its own
  branch there (and in the current-delegate `load_rooms_per_room`).

Missing any of these leaves the new key orphaned across delegate rebuilds —
exactly the bug the per-room `ListRequest` legacy probe (freenet/river#345) was
added to prevent.

## The crate walk (freenet-migrate) — the second, parallel recovery path

Since freenet/river#398 phase 3 there are **two** migration mechanisms running
side by side, and this file historically described only the first:

1. the hand-rolled sweep documented above, which stays authoritative for the
   user-facing load states; and
2. the shared `freenet-migrate` crate's **walk**, which adds durable
   per-predecessor markers and the library's classification.

The walk is armed once per page load from the end of
`fire_legacy_migration_request`, after the `any_dispatched` determination and
gated on it (arming it when every send failed would burn the once-per-load
latch on a dead transport). It waits for the sweep's load workers to settle
before its first round-trip.

Its durable markers live in the CURRENT delegate's KV store under
`__migrate_pred_done__:<hex>` and `__migrate_pred_wip__:<hex>`, hex-encoded
because the delegate's own key handling is `String::from_utf8_lossy`, which
would alias two predecessors whose raw keys differ only in invalid UTF-8. These
markers are what make the walk idempotent across page loads: a predecessor with
a Done marker is never re-fetched.

Because both mechanisms probe the same legacy delegates with the same
correlation keys, a legacy `ListResponse` is routed to `migrate_legacy_per_room`
only when no waiter consumed it (`!completed`) — otherwise the walk is already
importing that delegate and a second concurrent migration would displace its
reads in the single-waiter registry. See the comment at that call site for the
counting argument and its two exceptions.

Release 2 retires the sweep; until then, treat the sweep as authoritative and
the walk as additive. The `freenet-migrate-adoption` skill covers the
call-site swap, the dual-running period and the parity test.

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
