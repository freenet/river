# River CLI Quick Start Guide

## Installation

```bash
# From the River repository root
cargo install --path cli
```

## Basic Usage

### 1. Test Connection
```bash
river debug websocket
```

### 2. Create a Room
```bash
river room create --name "My Room" --nickname "YourName"
```

This will output a room owner key (save this for later commands).

### 3. Create an Invitation
```bash
river invite create <room-owner-key>
```

This generates an invitation code to share.

### 4. Accept an Invitation
```bash
river invite accept <invitation-code>
```

### 5. Debug Commands
```bash
# Show contract key for a room
river debug contract-key <room-owner-key>

# Perform raw GET operation
river debug contract-get <room-owner-key>
```

## Output Formats

All commands support JSON output:
```bash
river -f json <command>
```

## Common Issues

1. **WebSocket connection fails**: Make sure Freenet is running (`freenet network`)
2. **PUT/GET timeouts**: This is the bug we're investigating - operations timeout after 30 seconds

## Environment Variables

- `RUST_LOG=debug` - Enable debug logging
- `RUST_LOG=info` - Enable info logging

Example:
```bash
RUST_LOG=debug river -d room create --name "Test" --nickname "User"
```