# Claude Guide for River

## Freenet Node Operation
- Use `freenet local` for routine River UI development and manual testing. Local mode exercises the app without spinning up multiple network peers.
- Use `freenet network` only when validating peer-to-peer sync or multi-node behaviour. Document the scenario when you switch modes.

## Project Overview
River is a decentralized group chat application built on Freenet and consists of:
- Rust + Dioxus web UI compiled to WebAssembly
- Smart contracts (room and web container) deployed on Freenet
- Delegates that execute contract logic and perform background tasks
- Shared `common/` crate with data types and crypto helpers used across UI and contracts

## Essential Commands

### Development
```bash
cargo make dev-example             # UI with example data, no Freenet connection
cargo make dev                     # Standard development server
cargo make build                   # Full release build
cargo make build-ui                # UI artifacts only
cargo make build-ui-example-no-sync# UI build with example data and no sync
```

### Testing
```bash
cargo make test                    # Full workspace tests
cargo make test-room-contract
cargo make test-web-container
cargo make test-common
cargo make test-chat-delegate
cargo make test-web-container-integration
```

### Local UI Testing with dx serve

For rapid UI iteration without publishing to Freenet:

```bash
# From the ui/ directory
cd ui

# Local only (127.0.0.1)
dx serve --port 8082 --features example-data,no-sync

# Accessible from other machines (0.0.0.0)
dx serve --port 8082 --addr 0.0.0.0 --features example-data,no-sync
```

**Features:**
- `example-data` - Populates UI with sample rooms, members, messages, and reactions
- `no-sync` - Disables Freenet sync (no WebSocket connection required)

**Tips:**
- dx serve auto-rebuilds on file changes, but sometimes needs manual restart
- Check `/tmp/dx-serve-new.log` for build errors if UI doesn't update
- Use `--addr 0.0.0.0` when testing from remote machines (e.g., technic → nova)
- Example data includes reactions on messages for testing the emoji picker UI

### Playwright UI Tests (REQUIRED before publishing)

**Always run Playwright tests before publishing to Freenet.** Republishing takes minutes, so catch layout issues locally first.

```bash
# One-time setup: install browsers
cargo make test-ui-playwright-setup

# Build UI with example data (no Freenet connection needed)
cargo make build-ui-example-no-sync

# Serve built files (do NOT use dx serve — it auto-rebuilds and can serve stale content)
cd target/dx/river-ui/release/web/public && python3 -m http.server 8082 &

# Run all tests across Chromium, Firefox, WebKit, mobile Chrome, mobile Safari
cd ui/tests && npx playwright test

# Run specific browser or test
npx playwright test --project=chromium
npx playwright test --project=webkit --grep "iframe"
npx playwright test --project=mobile-safari --grep "Mobile"
```

**Test coverage:**
- Desktop 1280px: 3-column layout, no overflow
- Tablet 768px: narrower sidebars via CSS clamp
- Breakpoint 767px: mobile mode (single panel)
- Mobile 480px: view switching (hamburger, members, back buttons)
- Mobile 320px: small screen readability
- Desktop recovery after mobile resize
- Sandboxed iframe embedding (matching Freenet gateway)

**Important Tailwind v4 note:** The `@source "../src/**/*.rs"` directive in `ui/assets/tailwind.css` is REQUIRED. Without it, Tailwind v4 won't scan Rust files for class names, and responsive utilities like `md:flex` won't be generated.

**Two test directories exist:**
- `ui/tests/` — Layout/visual tests against `dx build` with example data (runs in CI)
- `e2e-test/` — Integration tests against a real Freenet node (manual)

### Interactive Playwright MCP (for debugging and verification)

The Playwright MCP plugin is enabled in `.claude/settings.local.json`. Use it
to interactively test the UI against a running local node — no manual browser
needed.

**Testing against example data (no Freenet node required):**
```bash
# Build and serve with example data
cargo make build-ui-example-no-sync
cd target/dx/river-ui/release/web/public && python3 -m http.server 8082 &
```
Then use Playwright MCP tools:
1. `browser_navigate` → `http://127.0.0.1:8082/`
2. `browser_snapshot` → inspect DOM state, verify layout
3. `browser_click` / `browser_fill_form` → interact with UI elements
4. `browser_console_messages` → check for WASM panics or JS errors

**Testing against a local Freenet node (full integration):**
```bash
# Publish to local node first
./scripts/local-republish.sh
# Script outputs the URL, e.g.:
#   http://127.0.0.1:7510/v1/contract/web/{CONTRACT_ID}/
```
Then use Playwright MCP tools to navigate to the published URL. This tests
the full stack: WASM ↔ WebSocket ↔ Freenet node ↔ contract/delegate.

**Common verification tasks with Playwright MCP:**
- **After UI changes**: Navigate, take snapshot, verify layout renders correctly
- **After message send fixes**: Fill message input, click send, verify message appears
- **After crash fixes**: Navigate, send message, check `browser_console_messages` for panics
- **Mobile simulation**: Use `browser_resize` to test responsive breakpoints (767px, 480px, 320px)
- **Debug overlay**: Navigate to `?debug=1` URL, verify overlay appears and logs render

**When to use Playwright MCP vs Playwright test suite:**
- **MCP** (interactive): Exploratory testing, debugging specific issues, verifying a fix before publishing
- **Test suite** (`npx playwright test`): Regression testing across all browsers/viewports before publishing

### Code Quality
```bash
cargo make clippy
cargo fmt
```

### Publishing & Verification

**Quick publish (when `cargo make publish-river` works):**
```bash
cargo make publish-river                    # Publish release build to Freenet
```

**Manual publish (when automated publish fails):**

The web container contract requires signed metadata with a version number strictly higher than the current published version. The version is tracked by a committed counter at `published-contract/contract-version.txt`. `cargo make sign-webapp` reads it, increments it, writes it back, and signs with the new value. After a successful publish, commit the bumped file.

When `cargo make publish-river` fails for a non-version reason and you need to drive the steps by hand:

1. **Increment the counter** (the automated path normally does this for you):
   ```bash
   current=$(cat published-contract/contract-version.txt)
   version=$((current + 1))
   echo "$version" > published-contract/contract-version.txt
   ```

2. **Build and sign with that version:**
   ```bash
   cargo make compress-webapp
   target/native/x86_64-unknown-linux-gnu/release/web-container-tool sign \
     --input target/webapp/webapp.tar.xz \
     --output target/webapp/webapp.metadata \
     --parameters target/webapp/webapp.parameters \
     --version $version
   ```

3. **Publish to local node:**
   ```bash
   fdev -p 7509 publish \
     --code published-contract/web_container_contract.wasm \
     --parameters published-contract/webapp.parameters \
     contract \
     --webapp-archive target/webapp/webapp.tar.xz \
     --webapp-metadata target/webapp/webapp.metadata
   ```

4. **Commit the bumped counter** alongside whatever other commits the publish included.

**Important notes:**
- The **parameters file** (`published-contract/webapp.parameters`) determines the contract ID — always use the committed one to get `raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv`.
- The **metadata** contains the signature and version — regenerate it with each publish.
- Version numbers must be strictly increasing. The `published-contract/contract-version.txt` counter is now the canonical source — never base the version on `date +%s / 60` (the previous scheme), as a single past clock-skew incident makes the on-network version stick ahead and the timestamp-derived value can never catch up. (2026-05-16: on-network was 30000208, timestamp gave 29649402, publish rejected.)
- Version-number gaps are fine; the contract enforces monotonicity, not contiguity. If you bump the counter and the publish fails, just retry — the next publish will use the next value and still be strictly-greater.
- The signing key is in `~/.config/river/web-container-keys.toml`.

**Verify deployment:**
```bash
curl -s http://127.0.0.1:7509/v1/contract/web/raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv/ | grep -o 'Built: [^<]*' | head -1
```

**Contract ID:** `raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv`

## Architecture Highlights
1. `common/`: shared state types (`RoomState`, `Member`, `Message`, `Invitation`) and cryptography helpers.
2. `contracts/room-contract`: manages room membership, permissions, and message history.
3. `contracts/web-container-contract`: serves the compiled UI as a Freenet contract asset.
4. `delegates/chat-delegate`: handles chat-specific workflows and background tasks.
5. `ui/`: Dioxus UI, including `example-data` and `no-sync` modes for offline testing.

## In-Room Direct Messages

End-to-end-encrypted DMs between two members of the same room, carried
inside `ChatRoomStateV1` (NOT a separate contract). The earlier
inbox-contract attempt (#234) was reverted in #238 in favour of this
design.

- Types and validation live in `common/src/room_state/direct_messages.rs`.
- Each DM is sender-signed over canonical bytes prefixed by domain tag
  `b'M'`; recipient purge envelopes use `b'P'`. Per-recipient purge
  envelopes are monotonically versioned (Configuration pattern) and the
  tombstone set is BLAKE3-derived `PurgeToken`s, which prevents
  signature-grinding attacks against tombstones.
- Bans are NOT enforced in `DirectMessagesV1::verify` - instead,
  `ChatRoomStateV1::post_apply_cleanup` sweeps DMs whose sender or
  recipient is now banned or no longer a member. This matches
  `MessagesV1`'s precedent and keeps `verify` stable across ban-state
  changes. Without this split, adding a ban for a DM participant would
  silently make every peer's verify fail.
- Phase 1 (PR #240) added types + validation + 42 tests including CRDT
  commutativity, retroactive-tombstone, and JSON-round-trip.
- Phase 2 + 3 (PR #244, issue #243) added the consumer surfaces:
  - UI: `ui/src/components/direct_messages/` (thread modal opened from the
    member-info modal, inbox modal with unread badges, in-memory
    `DM_LAST_SEEN` per (room, peer)).
  - `riverctl dm send | list | purge` in `cli/src/commands/dm.rs`.
  - Shared `river-core` helpers
    (`compose_direct_message` / `open_direct_message` / `advance_recipient_purges`)
    so UI and CLI emit byte-identical wire bytes.
  - `seal_dm_for_recipient` / `unseal_dm_from_sender` in
    `common/src/ecies.rs` carry the per-message ECIES envelope. Distinct
    from the deterministic `encrypt_secret_for_member` because DM
    plaintext is attacker-controlled (random ephemeral + random nonce per
    call). `open_direct_message` is feature-gated on `ecies` so the
    room-contract WASM (which never decrypts) still builds.
- Phase 4 (PR #244 follow-up, issue #252 partial) added the share-invite-via-DM
  picker — member-info modal entry point only; the Invite-Member-modal
  "Send to a co-member" entry point and the cross-room "is target already
  a member" filter are deferred (per-room identities make the filter
  structurally infeasible without a global-identity layer):
  - `INVITE_VIA_DM_PICKER` global signal opens
    `ui/src/components/direct_messages/invite_via_dm_picker_modal.rs`,
    which lists every other room the local user is in and generates an
    invitation for the picked room (signed via
    `signing::sign_member_with_fallback` against the CANDIDATE room's key,
    not the current room).
  - `DM_DRAFT` global signal carries the pre-composed body to
    `DmThreadModalBody`, which drains it on mount, appending to any text
    the user has already typed (never overwriting).
  - `seed_dm_last_seen_if_needed` (called from `App()` via a
    `use_effect` that subscribes to `ROOMS`) seeds `DM_LAST_SEEN` from
    the max inbound DM timestamp per `(room, peer)` exactly once on
    first hydration. A one-shot `DM_LAST_SEEN_SEEDED` flag prevents
    re-seeding on every ROOMS update — if we re-seeded, every arriving
    inbound DM would be instantly marked seen and never surface as
    unread.
  - `BodyKind::{Plaintext, Placeholder}` in `dm_thread_modal.rs` routes
    placeholder strings (`"sent — ciphertext only"`,
    `"unable to decrypt: …"`) through a plain muted text node, skipping
    markdown — the markdown crate's autolinker otherwise mangled the
    `<scheme:...>` prefix into a broken anchor.
  - `invite_member_modal::get_invitation_base_url()` is `pub(crate)` so
    the picker can produce byte-identical invitation URLs. Any change
    to the URL format must touch one place.
- Phase 5 (PR #259, issue #256) added the **outbound-DM plaintext
  cache**. The room contract carries DM bodies as ECIES ciphertext
  only the recipient can decrypt, so without a side channel the
  sender's UI / `riverctl dm list` could only render their own sent
  DMs as the legacy `"sent — ciphertext only"` placeholder.
  Persisting plaintext in the chat delegate solves this:
  - **Wire format** in `common/src/chat_delegate.rs`:
    `OUTBOUND_DMS_STORAGE_KEY = b"outbound_dms"`,
    `OutboundDmStore { entries: Vec<OutboundDmEntry> }` — `Vec` not
    `HashMap` so JSON serialisation works per the "non-string map
    keys in JSON-serialized API types" bug-prevention pattern; JSON
    and CBOR round-trip tests pin both shapes.
  - **UI in-memory cache**:
    `OUTBOUND_DMS: GlobalSignal<OutboundDmsCache>` (HashMap-keyed by
    `(VerifyingKey, MemberId, PurgeToken)`) in
    `ui/src/components/direct_messages.rs`. Hydrated on startup by
    `fire_load_outbound_dms_request` and migrated from any legacy
    delegate via the existing legacy-probe loop.
  - **Render path**: both `DmThreadModalBody` (UI) and
    `riverctl dm list` (CLI) go through the shared pure helper
    `lookup_outbound_plaintext(cache, room, recipient, token)`.
    Cache hit → render plaintext; miss → legacy placeholder. The
    helper is unit-tested by
    `dm_outbound_lookup_returns_plaintext_on_hit` /
    `…_returns_err_on_miss` — regression pinning for #256.
  - **Save path**: `save_outbound_dm()` defers the cache insert,
    enforces `MAX_DM_MESSAGES_PER_PAIR` eviction, and queues a
    single-flighted save via `save_outbound_dms_to_delegate` — a
    `OUTBOUND_DMS_SAVE_IN_FLIGHT`/`_DIRTY` atomic flag pair
    coalesces rapid sends into "current save + one final catch-up"
    rather than letting concurrent snapshots race and lose entries.
  - **Prune path**: `prune_outbound_dms_for_purges` (UI) and
    `prune_outbound_cache_for_room` (CLI) act ONLY on entries whose
    `(room, recipient, token)` appears in some recipient's
    `AuthorizedRecipientPurges` envelope — NEVER on the negative
    "no longer in `direct_messages.messages`" signal. Originally the
    prune used the negative signal and silently destroyed the cache
    on cold-start when `outbound_dms` hydrated before
    `direct_messages` state had caught up (PR #259 skeptical-review
    BLOCKING finding).
  - **Legacy migration**: see `.claude/rules/river-publish.md`
    "How Delegate Migration Works" for the storage-key probe set
    and the per-key gating rules. Adding any new top-level storage
    key requires extending both the probe loop in
    `fire_legacy_migration_request` AND the routing in
    `response_handler.rs`.
  - **CLI side**: persists the same `OutboundDmStore` shape into a
    sibling JSON file `outbound_dms.json` in the riverctl data dir
    (consistent with `rooms.json`'s plaintext-on-disk threat model
    — full-disk encryption is the user's responsibility).
- Phase 6 (PR #265, issue #261) added **hide-stale-DM-threads** —
  a local-only view filter that lets the user dismiss a DM thread
  from the left rail. Storage piggybacks **the same**
  `OUTBOUND_DMS_STORAGE_KEY = b"outbound_dms"` blob — `OutboundDmStore`
  grew a `hidden_threads: Vec<HiddenDmThreadEntry>` field with
  `#[serde(default)]` so pre-#261 bytes still decode. **Do not add a
  second top-level delegate storage key for hide state**: a new key
  would need its own probe in `fire_legacy_migration_request` and its
  own routing in `response_handler.rs` (per the legacy-migration note
  above), AND would split the multi-device save path into two writes
  that can race. The decision rationale lives on the Phase 5 prune
  path's "we only act on purge envelopes" comment in
  `chat_delegate::prune_outbound_dms_for_purges`. Filter helper
  `chat_delegate::is_thread_hidden` uses strict `<=`; the rail-side
  pure helper `dm_rail_section::filter_rail_entries` is pinned by
  `filter_rail_entries_*` tests, and the "click Hide again after
  revival must re-hide" branch is pinned by
  `hide_unhide_rehide_round_trip`.

## Private Room Support
- Messages, metadata, and member nicknames are encrypted with AES-256-GCM.
- Room secrets distributed with ECIES (X25519 + AES-256-GCM).
- Secret rotation has two converging paths (#228 PR 2 v2):
  - **UI synchronous fast-path** (`RoomData::rotate_secret`): runs while the owner
    is actively driving a state change — banning a member, clicking the manual
    "Rotate" button. Synchronous because we need the next owner-sent message to
    use the new key before the just-banned member can decrypt it.
  - **Delegate asynchronous catch-up** (`chat-delegate::handle_contract_notification`):
    runs when the UI isn't actively driving — auto-prune from message lifecycle,
    peer state updates received in the background. Triggered by
    `ContractNotification` from the runtime when a subscribed contract's
    state changes. Owner does NOT need the UI open.
  - Both paths derive the new secret deterministically via
    `river_core::key_derivation::derive_room_secret(seed, owner_vk, new_version)`,
    so they produce **byte-identical** secrets for the same target version.
    Concurrent rotation by both paths therefore converges via the contract's
    duplicate-version dedup in `RoomSecretsV1::apply_delta` (`secret.rs:140-145`).
  - The previous "weekly rotation" trigger was removed — it only fired while
    the UI was open, which defeated the point of a scheduled rotation.
- **In-memory secret repopulation** (#251): `room_data.secrets:
  HashMap<u32, [u8; 32]>` is `#[serde(skip)]` and must be rebuilt from
  `room_state.secrets.encrypted_secrets` after EVERY network state
  ingestion — initial GET, refresh/suspension GET, delegate-load merge,
  `apply_delta`, and full-state `update_room_state`. The helper
  `RoomData::repopulate_secrets_from_state` is the single source of
  truth; any new ingestion path MUST call it (the
  `repopulate_secrets_call_sites_pinned` test pins the existing call
  sites by source-grep so dropping one fails CI). Skipping the helper
  causes the bug from #251: newly-joined private-room members render
  `[Encrypted message - secret vN not available]` until they
  hard-refresh, because the back-filled blob arrives in a *subsequent*
  state update that the in-memory map never sees.
- Key files:
  - `common/src/room_state/privacy.rs`, `secret.rs`, `configuration.rs`
  - `common/src/key_derivation.rs`
  - `ui/src/util/ecies.rs`, `ui/src/room_data.rs`
  - `delegates/chat-delegate/src/subscription.rs`
  - `common/tests/private_room_test.rs`

## Delegate & Contract WASM Migration

When delegate or contract WASM changes (due to code changes in `delegates/`, `contracts/`, or `common/`),
the delegate/contract key changes. Without migration, existing users lose room data.

### Single Source of Truth: `legacy_delegates.toml`

All legacy delegate entries are defined in `legacy_delegates.toml` at the repo root.
This file is the **only** place migration entries are managed. The UI's build.rs generates
Rust code from it at compile time. CI reads it directly for validation.

### Upgrade Workflow

When you change code that affects delegate or contract WASM:

```bash
# 1. Add old delegate hash to migration registry
cargo make add-migration

# 2. Build new WASMs and copy to all committed locations
cargo make sync-wasm

# 3. Run migration tests
cargo test -p river-core --test migration_test

# 4. Verify UI compiles with new generated code
cargo check -p river-ui --target wasm32-unknown-unknown --features no-sync

# 5. Commit everything
git add legacy_delegates.toml ui/public/contracts/ cli/contracts/
git commit -m "fix: update WASMs with delegate migration entry"
```

### Validation

- **`cargo make check-migration`** — local check: builds delegate WASM and verifies migration entry exists if hash changed
- **`cargo test -p river-core --test migration_test`** — validates TOML entries: correct hex, 32-byte keys, delegate_key = BLAKE3(code_hash)
- **CI `check-delegate-migration` workflow** — builds base and PR WASMs, verifies old hash is in `legacy_delegates.toml`
- **CI `check-cli-wasm` workflow** — verifies `ui/public/contracts/` and `cli/contracts/` WASMs are in sync

### Key Files

| File | Purpose |
|------|---------|
| `legacy_delegates.toml` | Single source of truth for migration entries |
| `ui/build.rs` | Generates Rust const array from TOML at compile time |
| `ui/src/components/app/chat_delegate.rs` | Uses generated `LEGACY_DELEGATES` for runtime migration |
| `scripts/check-migration.sh` | Local + CI migration validation |
| `scripts/add-migration.sh` | Computes keys and appends entry to TOML |
| `scripts/sync-wasm.sh` | Builds all WASMs and copies to committed locations |
| `common/tests/migration_test.rs` | Validates TOML entries are well-formed |

### Technical Details
- **Delegate key formula**: `BLAKE3(BLAKE3(wasm) || params)` — both steps use BLAKE3
- **DelegateKey equality** checks BOTH `key` AND `code_hash` fields
- **WASM on disk is versioned**: `store_delegate()` wraps raw WASM with `to_bytes_versioned()`. The code_hash in `.reg` files is authoritative.
- **WASM committed in 3 places**: `ui/public/contracts/`, `cli/contracts/`, and `target/` (build output). Use `cargo make sync-wasm` to keep them in sync.

## Testing Notes
- Run `cd common && cargo test private_room` when modifying encryption or secret distribution.
- Use `cargo make test` before every PR to ensure all components still build and pass tests.

## State Authorization Rule

**Every piece of data in contract state must be cryptographically authorized. Never accept
unauthorized data into state, even as a "temporary" or "lenient" measure.**

- Messages must have a valid signature from a verified member at the time they are added
- Members must have a valid invitation chain back to the room owner
- Bans must be authorized by the room owner
- Verification must happen at insertion time — never defer verification to "when the data arrives later"

If a delta would introduce data that cannot be verified (e.g., a message whose author is not in
the members list), the fix must ensure the authorization data is included in the delta (e.g.,
include the member entry alongside the message), NOT relax verification to accept unauthorized data.

Relaxing verification creates security holes that are exploitable by malicious peers. A contract
that accepts unverified messages is vulnerable to spam, impersonation, and state pollution.

A key benefit of fully-authorized state: it enables **permissionless contract migration**. When
contract WASM changes, anyone can migrate state from the old contract to the new one because
the state is self-validating (see Contract Upgrade below).

## Contract Upgrade (WASM changes)

When the room contract WASM changes, the contract key changes (`key = BLAKE3(WASM_hash || params)`).
Both the UI and riverctl detect this automatically via `regenerate_contract_key()`.

**Because all state is cryptographically self-authorized, contract migration is permissionless:**
- ANY node (not just the room owner) can GET state from the old contract key and PUT it to the
  new contract key. The new contract validates all signatures and accepts it.
- The room owner does NOT need to be online or take any special action.
- The `OptionalUpgradeV1` pointer on the old contract is a courtesy for clients still running
  old versions — it tells them where the new contract lives. But updated clients already know
  the new key because they have the new WASM bundled.

**Upgrade flow for an updated client:**
1. On load, `regenerate_contract_key()` detects old_key != new_key
2. Client subscribes to the new contract key
3. Client GETs state from the old key and PUTs/merges it to the new key
4. If room owner: also sends an `OptionalUpgradeV1` pointer on the old key for stragglers

**This only works if:**
- The state format is backwards-compatible (see below)
- All state data is cryptographically authorized (see above)

## Backwards Compatibility Rule

`ChatRoomStateV1` and all sub-types must remain backwards-compatible:
- New fields must use `#[serde(default)]`
- Never remove or rename existing fields
- Never change serialization format of existing fields
- If a breaking change is truly needed, create a V2 type with explicit migration (separate project)

This ensures any client can re-PUT old state bytes and the new WASM's `validate_state()` accepts it,
which is critical for the permissionless contract migration system described above.

## Dioxus WASM Signal Safety Rules

The UI runs as single-threaded WASM. Firefox mobile runs Dioxus signal subscriber
notifications synchronously during Drop, causing `RefCell already borrowed` panics.
These rules prevent re-entrant borrow crashes.

### Always use `try_read()` for reactive signal reads

```rust
// WRONG — panics if signal is being written
let rooms = ROOMS.read();

// RIGHT — returns Err instead of panicking
let Ok(rooms) = ROOMS.try_read() else { return; };
```

**IMPORTANT:** In Dioxus 0.7.x, `try_read()` does NOT register signal subscriptions
when it returns `Err`. The subscription is registered only on the success path
(after the borrow succeeds). This means a `use_memo` that hits `try_read() -> Err`
will NOT be notified of future signal changes — it permanently stops re-evaluating.

To mitigate: ensure signal mutations happen in clean execution contexts (via
`crate::util::defer()`) so `try_read()` never encounters a concurrent borrow.
Also, memos that read multiple signals (e.g., `CURRENT_ROOM.read()` + `ROOMS.try_read()`)
get a backup subscription from the non-try signal.

### Never call `spawn_local` inside a polled future

Use `safe_spawn_local()` (in `util.rs`) which defers via `setTimeout(0)`:

```rust
// WRONG — re-entrant Task::run() panic on Firefox at singlethread.rs:132
wasm_bindgen_futures::spawn_local(async { ... });

// RIGHT
crate::util::safe_spawn_local(async { ... });
```

### Never mutate signals inside `spawn_local` or event handlers

Signal mutations (`ROOMS.with_mut()`, `ROOMS.write()`, `CURRENT_ROOM.write()`, etc.)
must always be wrapped in `crate::util::defer()` when called from `spawn_local` tasks
or synchronous event handlers (`onclick`, etc.). This is required for TWO reasons:

1. **RefCell re-entrancy**: Signal write Drop handlers fire subscriber notifications
   synchronously. Those notifications poll memos that call `try_read()` on the same
   signal — panics if the write guard's RefCell borrow is still held. `setTimeout(0)`
   breaks the call stack so no borrows are active.

2. **Missing Dioxus scope**: `wasm_bindgen_futures::spawn_local` tasks run without a
   Dioxus scope on the `scope_stack`. Signal subscriber notifications call
   `current_scope_id()` which panics on an empty scope_stack (`runtime.rs:223`).
   Our `defer()` uses `runtime.in_scope(ScopeId::ROOT, f)` to push both the runtime
   and a root scope before executing the closure.

**IMPORTANT**: `defer()` depends on `capture_runtime()` being called at app startup
(in `App()` component). Without it, deferred closures have no runtime to push and
GlobalSignal access panics with "Must be called from inside a Dioxus runtime."

```rust
// WRONG — panics at runtime.rs:223 (empty scope_stack) and/or
//         runtime.rs:280 (RefCell already borrowed)
spawn_local(async {
    ROOMS.with_mut(|rooms| { /* mutate */ });
});

// ALSO WRONG — onclick handlers trigger the same RefCell panic
onclick: move |_| {
    ROOMS.write().map.remove(&key);
};

// RIGHT — defer mutation to clean execution context with runtime+scope
spawn_local(async {
    // ... async work (signing, etc.) ...
    crate::util::defer(move || {
        ROOMS.with_mut(|rooms| { /* mutate */ });
        crate::components::app::mark_needs_sync(key);
    });
});

// RIGHT — onclick with defer
onclick: move |_| {
    crate::util::defer(move || {
        ROOMS.write().map.remove(&key);
    });
};
```

**Ordering caveat**: `defer()` schedules via `setTimeout(0)`, so the closure runs
asynchronously. Code after `defer()` executes BEFORE the deferred closure. If you
need data from a signal mutation for subsequent code, extract it before deferring:

```rust
// WRONG — signing_keys will be empty because ROOMS merge hasn't happened yet
crate::util::defer(move || { ROOMS.with_mut(|r| r.merge(loaded_rooms)); });
let signing_keys = ROOMS.with(|r| /* read signing keys */); // reads pre-merge state!

// RIGHT — extract data before moving into defer
let signing_keys = loaded_rooms.iter().map(|r| r.signing_key()).collect();
crate::util::defer(move || { ROOMS.with_mut(|r| r.merge(loaded_rooms)); });
```

See `defer()` in `util.rs`, `capture_runtime()` in `util.rs`, `mark_needs_sync()` in `app.rs`.

### Never use raw setTimeout for signal mutations

Always use `crate::util::defer()` instead of manual `web_sys::window().set_timeout_with_callback()`.
Our `defer()` pushes the Dioxus runtime and root scope via `runtime.in_scope(ScopeId::ROOT, f)`.
Raw setTimeout runs without any Dioxus context, so GlobalSignal access panics.

### Never defer signal clears in `use_effect`

Signal clears that the effect subscribes to must be synchronous. Deferring
causes an infinite loop (set remains non-empty → effect re-runs → defers
clear → effect re-runs...).

## PR Expectations
- Follow Conventional Commit style for PR titles (e.g., `fix(ui): correct room timestamp format`).
- Include a brief description of test coverage in the PR body.
- When touching contracts, note any required redeploy steps.

