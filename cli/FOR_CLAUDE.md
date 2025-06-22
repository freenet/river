# River CLI - Information for Claude

## Overview

This is a command-line interface for River, a decentralized chat application built on Freenet. The CLI was created to help debug a synchronization issue where PUT and GET operations timeout when communicating with the Freenet network.

## Key Files

- `cli/src/main.rs` - Entry point and command structure
- `cli/src/api.rs` - WebSocket client and Freenet API interactions
- `cli/src/commands/` - Command implementations
- `cli/REPRODUCE_FREENET_BUG.md` - Steps to reproduce the bug

## The Bug

River needs to store chat room data in Freenet's decentralized network. The issue:
1. PUT requests (storing data) timeout after 30 seconds
2. GET requests (retrieving data) timeout after 30 seconds  
3. WebSocket connection works fine - it's the contract operations that fail

## How to Test

1. **Ensure Freenet is running in network mode**:
   ```bash
   pkill -f freenet
   RUST_LOG=info freenet network > ~/freenet.log 2>&1 &
   ```

2. **Install the CLI**:
   ```bash
   cargo install --path cli
   ```

3. **Test the bug**:
   ```bash
   # This works - WebSocket is fine
   river debug websocket
   
   # This times out after 30s - demonstrates PUT issue
   river room create --name "Test" --nickname "Alice"
   
   # If you had a room, this would timeout too - demonstrates GET issue
   river debug contract-get <room-owner-key>
   ```

## Architecture Notes

- Uses `freenet-stdlib`'s native WebApi (not WASM)
- Reuses data structures from `river-common`
- Sends requests via WebSocket to local Freenet node
- Contract operations use CBOR serialization
- 30-second timeout added to prevent indefinite hangs

## Current Status

The CLI successfully:
- Connects to Freenet via WebSocket
- Formats and sends PUT/GET requests
- But never receives responses, leading to timeouts

This minimal reproduction case helps isolate the issue to Freenet's contract operation handling rather than River's code.