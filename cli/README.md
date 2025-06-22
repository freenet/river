# River CLI

Command-line interface for River chat on Freenet. This tool allows you to interact with River chat rooms without using the web interface, making it ideal for debugging, automation, and testing.

## Features

- Create and manage chat rooms
- Generate and accept invitations
- Send and receive messages (coming soon)
- Debug contract operations
- Support for both human-readable and JSON output

## Installation

```bash
cargo install --path cli
```

## Usage

See [QUICK_START.md](QUICK_START.md) for basic usage.

See [REPRODUCE_FREENET_BUG.md](REPRODUCE_FREENET_BUG.md) for debugging the synchronization issue.

## Architecture

The CLI reuses core components from the River web UI:
- `river-common` - Shared data structures
- `freenet-stdlib` - WebSocket client for Freenet communication
- Contract WASM from the UI's public folder

## Commands

- `river room` - Room management
- `river invite` - Invitation handling  
- `river message` - Messaging (coming soon)
- `river member` - Member management (coming soon)
- `river debug` - Debugging tools

Run `river --help` for full command documentation.