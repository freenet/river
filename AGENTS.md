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

The web container contract requires signed metadata with a version number higher than the current published version. When `cargo make publish-river` fails with version or network errors, use this workflow:

1. **Check current version** (by attempting to publish or checking error messages)

2. **Build and sign with correct version:**
   ```bash
   # Build the UI
   cargo make compress-webapp

   # Sign with version higher than current (check error message for current version)
   target/native/x86_64-unknown-linux-gnu/release/web-container-tool sign \
     --input target/webapp/webapp.tar.xz \
     --output target/webapp/webapp.metadata \
     --parameters target/webapp/webapp.parameters \
     --version <CURRENT_VERSION + 1>
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

**Important notes:**
- The **parameters file** (`published-contract/webapp.parameters`) determines the contract ID - always use the committed one to get `raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv`
- The **metadata** contains the signature and version - regenerate it with each publish
- Version numbers must be strictly increasing - check error messages for current version
- The signing key is in `~/.config/river/web-container-keys.toml`

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

## Private Room Support
- Messages, metadata, and member nicknames are encrypted with AES-256-GCM.
- Room secrets distributed with ECIES (X25519 + AES-256-GCM).
- Secret rotation happens manually (UI button), automatically on user ban, and weekly via scheduled checks.
- Key files:
  - `common/src/room_state/privacy.rs`, `secret.rs`, `configuration.rs`
  - `ui/src/util/ecies.rs`, `ui/src/room_data.rs`
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
`setTimeout(0)` deferral) so `try_read()` never encounters a concurrent borrow.
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

### Never mutate signals inside `spawn_local`

Move signal mutations out of async tasks via `setTimeout(0)`:

```rust
// WRONG — triggers re-entrant borrow in Firefox
spawn_local(async {
    ROOMS.with_mut(|rooms| { /* mutate */ });
});

// RIGHT — defer mutation to clean execution context
#[cfg(target_arch = "wasm32")]
{
    let cb = Closure::once_into_js(move || {
        ROOMS.with_mut(|rooms| { /* mutate */ });
    });
    web_sys::window().unwrap()
        .set_timeout_with_callback(&cb.into()).ok();
}
```

See `mark_needs_sync()` in `app.rs` and `safe_spawn_local()` in `util.rs`.

### Never defer signal clears in `use_effect`

Signal clears that the effect subscribes to must be synchronous. Deferring
causes an infinite loop (set remains non-empty → effect re-runs → defers
clear → effect re-runs...).

## PR Expectations
- Follow Conventional Commit style for PR titles (e.g., `fix(ui): correct room timestamp format`).
- Include a brief description of test coverage in the PR body.
- When touching contracts, note any required redeploy steps.

