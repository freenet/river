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
cargo test -p river-ui --bins      # river-ui crate native unit tests (CI-gated)
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

```bash
cargo make publish-river                    # Publish release build to Freenet
```

The web container contract requires signed metadata with a version
number strictly higher than the current published version. The version
is tracked by a committed counter at
`published-contract/contract-version.txt`; `cargo make sign-webapp`
increments it. Commit the bumped counter alongside other publish
artifacts.

**Contract ID:** `raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv`

**Verify deployment:**
```bash
curl -s http://127.0.0.1:7509/v1/contract/web/raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv/ | grep -o 'Built: [^<]*' | head -1
```

Full publish workflow (including delegate-migration coupling, manual
fallback when `publish-river` fails, the legacy-migration probe gate,
and the version-number policy) lives in
**`.claude/rules/river-publish.md`**.

## Architecture Highlights
1. `common/`: shared state types (`RoomState`, `Member`, `Message`) and cryptography helpers. (`Invitation` is NOT here — it is a `ui/`-only type, with a separate copy in `cli/`; it is not compiled into contract/delegate WASM.)
2. `contracts/room-contract`: manages room membership, permissions, and message history.
3. `contracts/web-container-contract`: serves the compiled UI as a Freenet contract asset.
4. `delegates/chat-delegate`: handles chat-specific workflows and background tasks.
5. `ui/`: Dioxus UI, including `example-data` and `no-sync` modes for offline testing.

## In-Room Direct Messages

End-to-end-encrypted DMs between two members of the same room, carried
inside `ChatRoomStateV1` (NOT a separate contract). Wire-format invariants,
the outbound-plaintext cache, and the archive (hide) state are documented
in **`.claude/rules/direct-messages.md`** — read it before touching anything
under `common/src/room_state/direct_messages.rs`,
`ui/src/components/direct_messages/`, or `cli/src/commands/dm.rs`.

## Private Room Support

Messages, metadata, and member nicknames are encrypted with AES-256-GCM.
Room secrets are distributed via owner-signed `encrypted_secrets` blobs
in the room contract (ECIES-wrapped per member) AND embedded in the
`Invitation` artifact so a new invitee can read immediately on join.
The contract blob is authoritative and supersedes the invitation-carried
copy.

The full invariants — secret rotation paths (UI fast-path vs delegate
catch-up), the shared `build_rotation_encrypted_secrets` helper, the
`post_apply_cleanup` encrypted_secrets exemption, the `member_info`
coupling rule, in-memory `repopulate_secrets_from_state`, and the
riverctl parity surface — are documented in
**`.claude/rules/private-rooms.md`**. Read it before touching anything
under `common/src/room_state/`, `common/src/key_derivation.rs`,
`ui/src/room_data.rs`, or `cli/src/private_room.rs`.

## Delegate & Contract WASM Migration

Changes to delegate or contract WASM (anything under `delegates/`,
`contracts/`, or `common/`, plus most Cargo.toml/Cargo.lock changes)
alter the delegate/contract key. Without a migration entry, **users lose
all room data**.

The full workflow — `cargo make add-migration` BEFORE rebuilding, the
two single-source-of-truth TOMLs, CI validation, and the key files —
lives in **`.claude/rules/delegate-migration.md`**. Read it before
publishing any change that touches those paths. The publish-side
counterpart is **`.claude/rules/river-publish.md`**.

## Testing Notes
- Run `cd common && cargo test private_room` when modifying encryption or secret distribution.
- Use `cargo make test` before every PR to ensure all components still build and pass tests.

## UI Test IDs (`data-testid`)

Stable hooks for automation (Playwright, debugging, external tools). Dioxus'
`data-dioxus-id` attributes are render-order indices that change between
renders, so automation MUST target `data-testid` instead (freenet/river#25).

Naming convention:
- **kebab-case**, scoped by surface: `<surface>-<role>` (e.g.
  `create-room-name-input`, `send-message-button`, `member-info-modal`).
- **List containers** get a plain id: `room-list`, `member-list`.
- **List items** use the entity-ID pattern `<entity>-item-{id}` so a specific
  row is addressable: `room-item-{base58(owner_vk)}`,
  `member-item-{member_id}`. Selecting any item: `[data-testid^="room-item-"]`.

Test IDs are additive markup only — never change rendering or logic. When
adding a new interactive surface, give it a `data-testid` following the above.
Existing coverage spans the room list + items, member list + items, the
create-room / edit-room / invite-member / member-info / receive-invitation
modals and their primary inputs/buttons, and the message input + send button.

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

The UI runs as single-threaded WASM and Firefox mobile fires Dioxus
signal subscriber notifications synchronously during Drop. There is a
small set of strict rules (use `try_read()`, never `spawn_local` inside
a polled future, always wrap signal mutations from spawn_local / event
handlers in `crate::util::defer()`, never raw `setTimeout`, never defer
clears in `use_effect`, never `use_memo` against non-signal values in
always-mounted components) that prevent re-entrant `RefCell` panics and
empty-scope_stack crashes.

Full rules with WRONG/RIGHT examples live in
**`.claude/rules/dioxus-signal-safety.md`**. Read it before touching
anything under `ui/src/`.

## PR Expectations
- Follow Conventional Commit style for PR titles (e.g., `fix(ui): correct room timestamp format`).
- Include a brief description of test coverage in the PR body.
- When touching contracts, note any required redeploy steps.

