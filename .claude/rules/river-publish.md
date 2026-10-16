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
git add legacy_delegates.toml pointer-records.toml ui/public/contracts/ cli/contracts/
git commit -m "fix: <description> with delegate migration"
cargo make build
cargo make compress-webapp
cargo make publish-river  # bumps published-contract/contract-version.txt
```

### Step 6: Commit the version bump through a PR, and verify

`main` is protected by a ruleset requiring four status checks (`build`,
`check-delegate-migration`, `check-room-contract-migration`,
`check-pointer-freshness`), so `git push origin main` is rejected with `GH013`.
The bumped counter lands through a small PR, like #686 and #701.

```bash
# `cargo make sign-webapp` (run transitively by publish-river) incremented
# the counter. Commit it — EVEN IF the publish reported a failure. See
# "Version policy" below: the counter is forward-only, because a reported
# failure is not evidence the state did not land.
curl -s http://127.0.0.1:7509/v1/contract/web/raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv/ | head -5
git switch -c chore/publish-counter-$(cat published-contract/contract-version.txt)
git add published-contract/contract-version.txt
git commit -m "chore(publish): bump webapp contract version to $(cat published-contract/contract-version.txt)"
git push -u origin HEAD
gh pr create --fill   # merge once the four required checks are green
```

Open that PR promptly. Until it merges, `main` holds a counter at or below the
version now live, and the next publish from `main` would sign a version the
network already has: a silent no-op, or a fork if the archive differs.

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
   `ListRequest`) for the **current** delegate only, WITHOUT registering it
   first (freenet/river#757: the node keeps a registered delegate, and
   re-uploading the WASM on every load gated room display). On the first load
   after a re-key the node does not have the new delegate and says so: a typed
   `missing delegate` error (core 0.2.137+), the untyped "delegate <key> not
   found" wording, or an empty reply (0.2.136 and earlier).
   `on_current_delegate_missing()` then registers it once per attempt, waits for
   the node's reply (freenet/river#709; bounded by `REGISTER_ACK_TIMEOUT_MS`;
   anything sent before that reply can run before the register) and re-lists.
   The outbound-DM load (`fire_load_outbound_dms_request()`) goes out only once
   the delegate is known present: after the `ListResponse`, or after that
   registration. Retry (`set_up_chat_delegate_registering()`) registers up
   front. None of this fires the legacy probes. (The startup load is
   List-driven, not a `GetRequest{rooms_data}`, because the per-room keys are
   dynamic and must be discovered.)
2. `LEGACY_DELEGATES` is generated at compile time from `legacy_delegates.toml`
   by `ui/build.rs`, which delegates the codegen itself to
   `freenet_migrate_build::codegen()`.
3. The current delegate's `ListResponse` is classified by `plan_load_from_keys`,
   and the same listing starts this page load's **migration journal** work
   (`migration_journal::start_journal_load`, below):
   - **Has `room:<vk>` keys** → `load_rooms_per_room` plain-GETs each slot
     (+ `rooms_meta`), reconstructs `Rooms`, and hydrates. Then
     `decide_per_room_load_action` decides whether to re-run the sweep to
     recover stranded rooms, from the journal's answer
     (`interrupted_import()` on the status this load's `begin` returned), or a
     `rooms_data` blob still stored beside the per-room keys:
     - *Not interrupted* (the common case) → no sweep re-run. Legacy probes
       never fire from this plan (#253).
     - *Interrupted* (a prior migration was cut short before it wrote every key
       — freenet/river#345 follow-up, #760) → after loading the partial set,
       re-run `migrate_current_blob_to_per_room()` once (armed via
       `arm_legacy_migration_recovery()`, which also clears the per-session
       attempt guard so same-session recovery isn't a no-op) to recover any
       stranded room with the sweep's full hydrate. The re-save is per-room CAS
       read-merge-write, so an already-present room merges rather than being
       clobbered.
   - **No per-room keys, only a legacy `rooms_data` blob** →
     `migrate_current_blob_to_per_room` reads it and re-saves as per-room keys
     (non-destructive — the blob is left as a rollback fallback).
   - **Nothing** → `fire_legacy_migration_request()`.

   There is **no "done" seal and no `localStorage` flag** any more
   (freenet/river#763). Both lived in `localStorage`, which throws in the
   gateway's sandboxed iframe, so in production neither ever persisted, and an
   interrupted migration stranded rooms for good (#760). The journal, stored
   in the current delegate, is the only authority.
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
6. **Every legacy merge the sweep makes is journal-gated** (#763):
   `sweep_may_merge` -> `migration_journal::gate_sweep_import` ->
   `note_app_import_started` runs before the merge, so a tab killed mid-save
   leaves an interrupted import that the next load's `begin` reports. `Flagged`
   merges; `AlreadyImported` merges too, unflagged, through the same ranked,
   tombstone-safe `merge_from_source` (a backstop for a completed import that
   was partial; a room left since stays left); any error skips that
   predecessor on this load. The per-room sweep passes an item count in the
   walk's unit (keys that answered with a value); the fixed-key arms pass none.

**The #253 ordering, now that the walk runs on every load.** The sweep's
fixed probes still fire only when the current delegate is empty (or for an
interrupted-migration recovery). The journaled walk, though, imports every
pending predecessor while the journal is incomplete, whatever the current
delegate holds (#763). That cannot clobber newer state: the walk waits for the
load's workers (the current delegate's own room load included) to settle
before it starts, every import goes through `Rooms::merge_from_source` at the
predecessor's generation rank, below the current delegate's (so a current
identity wins a conflict and a current tombstone keeps a room left), messages
CRDT-merge, and the save is a per-room CAS read-merge-write against what the
delegate already holds. Pinned by the ranked-merge tests in
`delegate_migration.rs` (`a_room_deleted_after_migration_is_not_resurrected`,
`identity_conflicts_resolve_by_generation_rank_not_arrival`).

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

## The crate walk and the completeness journal (freenet-migrate 0.7)

There are **two** importers running side by side, and since freenet/river#763
one durable authority over both:

1. the hand-rolled sweep documented above (fast, owns the user-facing load
   states and the full hydrate); and
2. the shared `freenet-migrate` crate's **journaled walk**
   (`delegate_migration.rs`), which imports every predecessor through River's
   own ranked merge and records what each one resolved to.

`migration_journal.rs` drives them:

- **`begin_migration_journal` on every page load**, from the current
  delegate's room listing, and lazily before any sweep merge that comes first.
- **The walk while the status `begin` returned is incomplete**, whatever the
  load plan (after an interrupted per-room write the plan is `PerRoom`, so the
  walk can no longer hang off the `ProbeLegacy` sweep). Rate-limited
  (freenet/river#761, `walk_cadence`) only while nothing is mid-import
  (except a predecessor whose short-index refusal is recorded) and every
  pending generation either answered a TYPED "missing delegate" on the last 3
  walks or stayed unresolved through 6 walks (silent or refused). It then
  runs on one load in 10 if every pending generation is in the first group,
  else one load in 4, counted under `__river_migration_walk_skips__` in the
  current delegate. Silence never counts as "not installed". The empty
  account's sweep is not rate-limited: it still probes every legacy
  generation on every load (#761).
- **The walk again after a sweep flag**, once the load is quiet, so the
  sweep's import is resolved in the same session.
- All journal calls are **serialised** (the request registry holds one waiter
  per correlation key).

What lives in the CURRENT delegate's KV store: the journal under
`\0freenet-migrate-journal/v1` (the crate's reserved namespace), the
per-predecessor markers `__migrate_pred_done__:<hex>` / `__migrate_pred_wip__:<hex>`
(hex because the delegate's key handling is `String::from_utf8_lossy`), and the
cadence counter. None of them is ever fetched, imported or counted when this
delegate later becomes a predecessor (`is_migration_bookkeeping_key`).

The marker store follows the crate's three-rule contract: a data in-progress
marker beside an empty done marker reads as in progress; done and in-progress
are separate keys; the in-progress marker is written by compare-and-swap, so no
session downgrades data to empty.

`fetch_secrets` never returns a short list (#592): it reads the predecessor's
RAW key index twice and refuses one that does not decode, two reads that
disagree, a listing that disagrees with it, an absent index beside a stored
fixed key, a probed fixed key or a room named by the predecessor's
`rooms_meta` (order, notification modes, cursor) that is stored but unlisted
(the common #592 shape: a short index rewritten by a later `rooms_meta`
store), and a listed key that answers "no value". A wrong refusal costs a
retry; a wrong short answer would be certified complete.

**End state of a refused predecessor.** A FROZEN generation whose index is
genuinely short is refused on every load: it stays pending (its
`absent_streak` stays 0; once it has 6 refused walks and every other pending
generation is settled too, the #761 cadence walks on one load in 4), and its
unlisted rooms do not migrate. The rooms its short index does list still
arrive through the sweep. The user is told only when importing an identity:
the gate reads `Unresolved` and offers "Import anyway". There is no general
notice, and no repair path short of reading the delegate's storage directly
(which the delegate API cannot do without `list_secrets`, a host import
nodes before 0.2.81 lack).

**The walk runs only over a complete current room set** (#768 review A1).
A load attempt records `mark_room_set_complete` only when every listed slot
of the CURRENT delegate and its `rooms_meta` answered and decoded (a listed
key with no value is a read fault, never a skip), from a `defer` queued after
the hydration it vouches for. A legacy probe's failure does not gate the
walk. A listed slot that stays unreadable gates the walk on every load (the
rail shows Retry); that is deliberate, since walking over it is #595. The walk driver captures the attempt, waits for the load
to settle, and aborts (re-arming the next attempt, without spending the walk
budget) if the load is still pending at the cap, a reconnect began a new
attempt, or the set is not complete; `LiveImportSeam` re-checks before every
merge, inside each deferred mutation, and before its flush. Merging older copies into a set that failed to load, then
saving, is #595's identity-overwrite chain.

**The sweep's journal gate** is retried with backoff; a final refusal marks
the load's fetch failure (Retry, not a false "no rooms") and leaves that
generation to the walk, which is not gated by it.

**Post-resolve top-up** (#768 review B1). An old tab on the previous build
keeps writing to the newest predecessor (V33) until it is closed. After the
journal resolves it, each load reads V33's raw key index (one GET) and, within
the window, re-lists it and merges rooms the current delegate does not know at
all, plus its outbound-DM entries (never its hidden threads), through the
ranked merge. The window (`__river_migration_topup_window__`) is 14 days or
30 top-ups that actually ran, and restarts whenever V33's index digest
changes. The top-up is its own driver request: it never forces a walk.
Only DM entries the cache does not already hold are merged, so an unchanged
predecessor costs no merge and no save. Only the NEWEST predecessor is topped
up: a room a tab on an older build (V32) creates after the journal resolved V32
stays in V32, not destroyed, and is not picked up.

**Archive entries across generations** (freenet/river#776). Un-archiving a DM
thread deletes its `hidden_threads` entry with no tombstone, so merging an
older generation's archives can bring back one the user removed in a newer
generation. Only the NEWEST predecessor holding archive data has its
`hidden_threads` merged (`newest_archive_holder`): every newer generation must
be known to hold no DM store (it exported none earlier in the same walk, or
the journal resolved it as not data-bearing, or it is typed "not installed").
A generation with rooms but no DM store counts as holding none: archiving and
un-archiving both save the store, so it holds no archive decision. A
generation resolved data-bearing in an EARLIER load counts as holding one,
because the journal does not record which items it held.
Otherwise that generation's archives are skipped, permanently: the journal
seals that generation in the same walk, so it is never imported again. On
nodes before 0.2.137 a missing delegate answers with silence, so a newer
generation that is not installed there makes older archives skip; that cost
applies only when a newer generation is silent. The walk runs newest-first,
so a newer answer is known first. Nothing is ever removed. The sweep and the
top-up never merge archives; the walk does, and it imports every
predecessor's DM store while the journal is incomplete. A wrong skip shows a
thread un-archived; it never loses a message. Accounts #772 already re-archived
are not healed (see the follow-up issue linked from #776). There is no "a newer
outbound DM supersedes the entry" repair: an archive's `hidden_at_ts` is the
newest INBOUND timestamp, not the archive time, so it would un-archive every
thread the user replied to and then archived.

**`rooms_meta` is the first item the walk writes** (freenet/river#802), so
the room order is in place before the rooms render one at a time. Two things
that need the predecessor's rooms merged first run at its flush instead
(`flush_predecessor`):

- the identity ranks its `identity_sources` record are lowered (below), before
  the save that persists them;
- if the predecessor merged any rooms, every order the walk has seen so far is
  merged once more, folded into one merge, newest generation first. A room
  still tombstoned when its order was first merged, then resurrected by this
  or an older generation's slot, gets its place back (re-merging only the
  current predecessor's order is not enough).

Do not move `rooms_meta` back to the end: the rail then shows key-byte order
until it lands.

**Migrated identities keep their source rank across loads** (#768 review
B3): `rooms_meta.identity_sources` records which legacy generation supplied a
room's `self_sk`, and the load restores it, so a newer generation answering
later still wins the identity conflict (and a newer generation's tombstone
still removes the room). Another tab's entries survive this tab's save, and an
imported predecessor's own entries are carried into the rank registry.

The **identity-import gate** (#414) is composed from the journal: a walk in
flight or a status not yet read waits; `begin` failing, a walk whose status is
unknown, `interrupted_import()`, or permanently rejected items block; and a
pending predecessor that has not been found "not installed" (silent, or
refused) reads `Unresolved` and waits, because an identity imported then would
outrank the newer one it brings later. Blocked and Unresolved both offer a
user override ("Import anyway") that ignores, never resolves, the answer.

**The chat delegate's key-value writes are crash-safe** (#768 round 2). The
empty index is created before any value is ever written, so a stored key never
exists without one; an absent index beside a stored key is a refused read
fault. Adding a new key or deleting a listed one records an intent under
`<origin>:::kv_intent` first, and every operation first rolls a recorded
intent forward, so a node restart between any two writes wedges nothing.

Because both importers probe the same legacy delegates with the same
correlation keys, a legacy `ListResponse` is routed to `migrate_legacy_per_room`
only when no waiter consumed it (`!completed`) — otherwise the walk is already
importing that delegate and a second concurrent migration would displace its
reads in the single-waiter registry. See the comment at that call site for the
counting argument and its two exceptions.

**Raise `RIVER_IMPORT_REVISION`** (`delegate_migration.rs`) with any change to
how the import path decodes items, or predecessors resolved with rejected items
stay resolved and their items stay missing. The `freenet-migrate-adoption`
skill covers the call-site swap, the dual-running period and the parity test.

## Migration Limitations

- Old delegate WASM must be in the node's delegate store (registered in a previous session)
- stdlib versions before ~0.1.34 used removed enum variants — old WASM can't execute on new runtime
- Host function API must be compatible between old and new WASM

## Common Mistakes

- **Wrong hash algorithm**: BLAKE3 not SHA256 for CodeHash
- **Forgetting migration**: Users lose all room data
- **Computing key AFTER changes**: Must run `add-migration` BEFORE changes alter the WASM
- **Not republishing riverctl**: Use `cargo make publish-all` when the room-contract WASM changes (riverctl does not embed the chat delegate)
- **Parameters file**: Always use `published-contract/webapp.parameters` (committed) — determines contract ID

## Contract ID

`raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv`
