# Reproducing River/Freenet Synchronization Bug with River CLI

This document explains how to use the River CLI tool to reproduce the PUT/GET timeout issue described in [River Issue #26](https://github.com/freenet/river/issues/26).

## Prerequisites

1. Install the River CLI:
   ```bash
   cd /path/to/river
   cargo install --path cli
   ```

2. Ensure Freenet is running in **network mode** (not local mode):
   ```bash
   # Kill any existing Freenet process
   pkill -f freenet
   
   # Start Freenet in network mode with logging
   RUST_LOG=info freenet network > ~/freenet.log 2>&1 &
   
   # Wait for connection
   sleep 10
   
   # Verify it has connected to peers
   fdev query
   ```
   
   You should see at least one peer connection. If not, wait longer or check ~/freenet.log.

## The Bug

River uses Freenet's decentralized storage to persist chat room state. The bug manifests as:
- PUT operations (storing data) timeout after 30 seconds with no response
- GET operations (retrieving data) also timeout after 30 seconds with no response
- This happens even though the WebSocket connection to the local Freenet node works fine

## Reproduction Steps

### Step 1: Test WebSocket Connection
First, verify the connection to your local Freenet node works:

```bash
river debug websocket
```

Expected output:
```
DEBUG: Testing WebSocket connection...
✓ WebSocket connection successful
```

### Step 2: Create a Room (PUT Operation)
Attempt to create a new chat room:

```bash
river room create --name "Test Room" --nickname "Alice"
```

Expected output (demonstrating the bug):
```
Creating room 'Test Room' with nickname 'Alice'...
Error: Timeout waiting for PUT response after 30 seconds
```

The CLI sends a PUT request to store the room contract, but Freenet never responds.

### Step 3: Try Again with Debug Logging
Run with debug logging to see more details:

```bash
RUST_LOG=debug river -d room create --name "Test Room" --nickname "Alice" 2>&1 | tee debug.log
```

You'll see the PUT request being sent but no response received.

### Step 4: Test GET Operation
If you somehow have a room created (e.g., from a previous attempt), you can test GET:

```bash
# First, get the room owner key from a successful creation
# For this example, let's say it's: 7oQfp6UHFDK4h7gWrPBkajDW2iKfuWPVnLmKBrr1YXwP

river debug contract-get 7oQfp6UHFDK4h7gWrPBkajDW2iKfuWPVnLmKBrr1YXwP
```

Expected output (demonstrating the bug):
```
DEBUG: Performing contract GET for room owned by: 7oQfp6UHFDK4h7gWrPBkajDW2iKfuWPVnLmKBrr1YXwP
Contract key: 9Xx6VzR8HSsG1h...
Error: Timeout waiting for GET response after 30 seconds
```

### Step 5: Test Invitation Flow (Alternative GET Test)
Create an invitation locally and try to accept it:

```bash
# This would only work if you had successfully created a room
# river invite create <room-owner-key>
# river invite accept <invitation-code>
```

The accept command performs a GET operation and will also timeout.

## What's Happening

1. **WebSocket Connection**: Works fine - we can connect to the local Freenet node
2. **Request Sending**: The River CLI successfully sends PUT/GET requests via WebSocket
3. **No Response**: Freenet accepts the requests but never sends a response
4. **Timeout**: After 30 seconds, the CLI gives up waiting

## Debugging Information

### Useful Commands

Show the contract key for a room (useful for debugging):
```bash
river debug contract-key <room-owner-key>
```

Monitor Freenet logs while testing:
```bash
tail -f ~/freenet.log | grep -E "(River|contract|WebSocket)"
```

Check JSON output for scripting:
```bash
river -f json room create --name "Test" --nickname "User"
```

### Expected Behavior

When working correctly:
1. PUT requests should receive a response confirming the contract was stored
2. GET requests should receive the contract state data
3. Both operations should complete in under a few seconds

### Current Behavior

1. Both PUT and GET requests timeout after 30 seconds
2. No error message from Freenet - just no response
3. The WebSocket connection remains active

## Technical Details

- River uses CBOR serialization for contract state
- Contracts are WASM modules that validate state changes
- Each room has a unique contract key derived from the owner's public key
- The CLI uses the same freenet-stdlib WebApi as the web UI

## Next Steps

This bug needs to be investigated at the Freenet level to understand why contract operations are not receiving responses. The River CLI provides a minimal test case without the complexity of the web UI.