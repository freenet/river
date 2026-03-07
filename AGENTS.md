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

## Delegate Migration

When delegate WASM changes (due to code changes in `delegates/chat-delegate/` or `common/`), the delegate key changes. Without a migration entry, existing users lose room membership.

### .reg File Format
- `byte[0]` = version (`01`)
- `bytes[1..33]` = code_hash (BLAKE3 of raw WASM)
- `bytes[33..37]` = params_len (little-endian u32)
- `bytes[37..]` = params bytes
- Filename = `base58(delegate_key)`

### Key Facts
- **DelegateKey equality** checks BOTH `key` AND `code_hash` fields — wrong code_hash means the node can't find the delegate even if key bytes match
- **WASM on disk is versioned**: `store_delegate()` wraps raw WASM with `to_bytes_versioned()`. Hashing `.wasm` files from the delegates directory gives DIFFERENT results than hashing raw WASM. The code_hash in `.reg` files is authoritative.
- **Delegate key formula**: `BLAKE3(BLAKE3(wasm) || params)` — both steps use BLAKE3
- **CI check**: The `check-delegate-migration` workflow detects WASM changes without corresponding `LEGACY_DELEGATES` updates

## Testing Notes
- Run `cd common && cargo test private_room` when modifying encryption or secret distribution.
- Use `cargo make test` before every PR to ensure all components still build and pass tests.

## Backwards Compatibility Rule

`ChatRoomStateV1` and all sub-types must remain backwards-compatible:
- New fields must use `#[serde(default)]`
- Never remove or rename existing fields
- Never change serialization format of existing fields
- If a breaking change is truly needed, create a V2 type with explicit migration (separate project)

This ensures any client can re-PUT old state bytes and the new WASM's `validate_state()` accepts it,
which is critical for the any-client contract migration system.

## PR Expectations
- Follow Conventional Commit style for PR titles (e.g., `fix(ui): correct room timestamp format`).
- Include a brief description of test coverage in the PR body.
- When touching contracts, note any required redeploy steps.

