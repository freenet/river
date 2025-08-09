# River CLI Quick Start Guide

## Installation

```bash
cargo install riverctl
```

## Basic Usage

### 1. Test Connection
```bash
riverctl debug websocket
```

### 2. Create a Room
```bash
riverctl room create --name "My Room" --nickname "YourName"
```

This will output a room owner key (save this for later commands).

### 3. Create an Invitation
```bash
riverctl invite create <room-owner-key>
```

This generates an invitation code to share.

### 4. Accept an Invitation
```bash
riverctl invite accept <invitation-code>
```

### 5. Debug Commands
```bash
# Show contract key for a room
riverctl debug contract-key <room-owner-key>

# Perform raw GET operation
riverctl debug contract-get <room-owner-key>
```

## Output Formats

All commands support JSON output:
```bash
riverctl -f json <command>
```

## Common Issues

1. **WebSocket connection fails**: Make sure Freenet is running (`freenet network`)
2. **Connection refused**: Ensure Freenet is running on the default port (50509)

## Environment Variables

- `RUST_LOG=debug` - Enable debug logging
- `RUST_LOG=info` - Enable info logging

Example:
```bash
RUST_LOG=debug riverctl -d room create --name "Test" --nickname "User"
```