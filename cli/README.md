# River CLI

Command-line interface for River decentralized chat on Freenet. This tool allows you to interact with River chat rooms without using the web interface, making it ideal for automation, testing, and server deployments.

## Features

- Create and manage chat rooms
- Generate and accept invitations
- Debug contract operations
- Support for both human-readable and JSON output
- Send and receive messages (coming soon)
- Member management (coming soon)

## Installation

```bash
cargo install riverctl
```

## Usage

See [QUICK_START.md](QUICK_START.md) for basic usage examples and getting started guide.

## Testing

### Freenet Integration Smoke Test (experimental)

The integration test at `tests/message_flow.rs` uses the `freenet-test-network`
crate to launch a local Freenet gateway plus two peers, then drives the River CLI
to create a room, exchange invitations, and send messages between two users.

Run it manually (it is ignored by default) from `river/main/cli`:

```bash
cargo test --test message_flow -- --ignored --nocapture
```

Prerequisites:

- `~/code/freenet/freenet-core/main` must exist (the test builds the Freenet
  binary from there)
- `freenet-test-network` dev-dependency will be fetched from crates.io automatically (no sibling checkout required)

Expect the test to fail today with the current contract serialization bug; it
exists to reproduce and debug the issue.

> **Heads up:** When you change the room contract or shared River types, rebuild
> the WASM and refresh the bundled copy with `cargo make sync-cli-wasm`. The CLI
> build now double-checks and will panic if the bundled file drifts from the most
> recently built artifact.

## Requirements

- A running Freenet node (accessible at `ws://127.0.0.1:7509`)
- Rust 1.70 or higher (for building from source)

## Architecture

The CLI uses core components from the River ecosystem:
- `river-core` - Core protocol and data structures
- `freenet-stdlib` - WebSocket client for Freenet communication
- Pre-built room contract WASM included in the package

## Commands

- `riverctl room` - Room management (create, list, info)
- `riverctl invite` - Invitation handling (create, accept)
- `riverctl debug` - Debugging tools for contract operations
- `riverctl message` - Messaging (coming soon)
- `riverctl member` - Member management (coming soon)

Run `riverctl --help` for full command documentation.
