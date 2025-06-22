# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

River is a decentralized group chat application built on Freenet. It uses:
- Rust with Dioxus (React-like) framework for the web UI
- Freenet smart contracts for decentralized state management
- WebAssembly compilation for both UI and contracts
- Tree-based invitation system for room membership

## Essential Commands

### Development
```bash
# Run UI with example data (no Freenet connection needed)
cargo make dev-example

# Run normal development server
cargo make dev

# Build everything in release mode
cargo make build

# Build UI only
cargo make build-ui

# Build with example data and no-sync for testing
cargo make build-ui-example-no-sync
```

### Testing
```bash
# Run all tests
cargo make test

# Run specific component tests
cargo make test-room-contract
cargo make test-web-container
cargo make test-common
cargo make test-chat-delegate

# Run integration tests
cargo make test-web-container-integration
```

### Code Quality
```bash
# Run clippy on all packages
cargo make clippy

# Format code
cargo fmt
```

### Publishing to Freenet
```bash
# Update published contract (commit changes after)
cargo make update-published-contract

# Publish River to Freenet
cargo make publish-river

# Publish in debug mode (IMPORTANT: Use this for testing)
RUST_MIN_STACK=16777216 cargo make publish-river-debug

# Verify River build time after publishing (CRITICAL step)
curl -s http://127.0.0.1:50509/v1/contract/web/BcfxyjCH4snaknrBoCiqhYc9UFvmiJvhsp5d4L5DuvRa/ | grep -o 'Built: [^<]*' | head -1
```

## Architecture Overview

The codebase consists of several interconnected components:

1. **common/** - Shared types and logic between UI and contracts
   - `room_state/` - Core chat room state management (members, messages, invitations)
   - Key types: `RoomState`, `Member`, `Message`, `Invitation`

2. **contracts/** - Freenet smart contracts
   - `room-contract/` - Manages chat room state and member permissions
   - `web-container-contract/` - Serves the web UI as a Freenet contract

3. **ui/** - Dioxus-based web interface
   - Uses reactive signals for state management
   - Communicates with Freenet via WebSocket API
   - Features: `example-data` (test data), `no-sync` (offline mode)

4. **delegates/** - Freenet delegates for contract logic
   - `chat-delegate/` - Handles chat-specific operations

## Key Development Patterns

### Contract-UI Communication
- UI sends updates via WebSocket to Freenet network
- Contracts validate and persist state changes
- State synchronization happens through contract queries

### Invitation System
- Tree-based structure where each member can invite others
- Invitations include cryptographic proofs of authorization
- Members inherit permissions from their inviters

### State Management
- Room state is the source of truth in contracts
- UI maintains local state synchronized with contracts
- All state changes must go through contract validation

## Important Conventions

From CONVENTIONS.md:
- Keep files under 200 lines
- Use flat module structure (foo.rs instead of foo/mod.rs)
- Organize code top-down (high-level first)
- Avoid nested signal borrows in Dioxus - extract values to locals first

## Build Requirements

- Rust with wasm32-unknown-unknown target
- cargo-make (`cargo install cargo-make`)
- Dioxus CLI (`cargo binstall dioxus-cli`)
- For Freenet deployment: fdev tool

## Common Pitfalls

1. **Dioxus Signal Borrows**: Don't call `write()` while holding `read()` on same signal
2. **WASM Target**: Many dependencies don't work with wasm32-unknown-unknown
3. **Contract Size**: Keep contracts small - they run on limited resources
4. **Feature Flags**: Remember to enable correct features for testing vs production

## Testing Strategy

- Unit tests for core logic in common/
- Integration tests for contract behavior
- UI testing with `example-data` feature for predictable state
- Full end-to-end testing requires Freenet deployment

## Debugging Guide

### Known Issues

1. **Invitation Bug (2025-01-18)**: Room invitations hang at "Subscribing to room..."
   - Root cause: Contract PUT/GET operations timeout on live network
   - Works in integration tests but fails in production
   - See debugging notes in freenet-core `freenet-invitation-bug.local/`

### WebSocket Connection
- River connects to Freenet at `ws://127.0.0.1:50509/ws/v1`
- Default WebSocket message size limit: 100MB (after fix in freenet v0.1.12+)
- Check connection status in browser DevTools Network tab

### Debugging Commands
```bash
# Monitor Freenet logs while testing River
tail -f ~/freenet-debug.log | grep -E "(River|contract|WebSocket)"

# Check if River is being served correctly
curl http://127.0.0.1:50509/v1/contract/web/BcfxyjCH4snaknrBoCiqhYc9UFvmiJvhsp5d4L5DuvRa/

# Test contract operations directly (requires contract-test tool)
cd contract-test && cargo run -- --get <contract-key>
```

### Key Files to Inspect When Debugging
- `ui/src/components/app/freenet_api/room_synchronizer.rs` - Handles room sync
- `ui/src/room_data.rs` - Room data structures
- `common/src/room_state/mod.rs` - Core room state logic
- `contracts/room-contract/src/lib.rs` - Contract validation logic

### Common Error Messages
- "WebSocket connection error: client error: operation timed out" - Contract operation timeout
- "Subscribing to room..." (hangs) - GET request for room data failing
- "Failed to parse room response" - Contract data corruption or version mismatch

## Performance Considerations

1. **Contract Size**: Room contracts grow with member/message count
2. **WebSocket Messages**: Large state updates can hit message size limits
3. **WASM Stack Size**: Set `RUST_MIN_STACK=16777216` for publish operations
4. **Browser Memory**: Large rooms may consume significant client memory

## Future CLI Tool
A command-line interface is planned (Issue #26) to enable:
- Easier debugging without browser UI
- Automated testing of contract operations
- Direct access to River functionality
- Better visibility into PUT/GET operations