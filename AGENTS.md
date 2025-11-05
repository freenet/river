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

### Code Quality
```bash
cargo make clippy
cargo fmt
```

### Publishing & Verification
```bash
cargo make update-published-contract        # Refresh published contract sources
cargo make publish-river                    # Publish release build to Freenet
RUST_MIN_STACK=16777216 cargo make publish-river-debug  # Debug publish
curl -s http://127.0.0.1:7509/v1/contract/web/<contract-id>/ | grep -o 'Built: [^<]*' | head -1
```
Replace `<contract-id>` with the current published ID documented in `README.md`.

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

## Testing Notes
- Run `cd common && cargo test private_room` when modifying encryption or secret distribution.
- Use `cargo make test` before every PR to ensure all components still build and pass tests.

## PR Expectations
- Follow Conventional Commit style for PR titles (e.g., `fix(ui): correct room timestamp format`).
- Include a brief description of test coverage in the PR body.
- When touching contracts, note any required redeploy steps.

