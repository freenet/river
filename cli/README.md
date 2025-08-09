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

## Requirements

- A running Freenet node (accessible at `ws://127.0.0.1:50509`)
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