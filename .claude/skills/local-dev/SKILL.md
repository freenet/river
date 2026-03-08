---
name: local-dev
description: Build, publish, and debug workflows for local development. Use when iterating on UI, debugging message sending, or testing on mobile devices.
---

# Local Development

Build, publish, and debug workflows. For general Freenet local node management,
contract publishing, and debugging patterns, see the `local-dev` skill in the
[freenet-agent-skills](https://github.com/freenet/freenet-agent-skills) plugin.

## Quick Start

### Prerequisites

```bash
which freenet fdev dx
rustup target add wasm32-unknown-unknown
```

### One-time setup

```bash
# 1. Build the web-container-tool (native, not cross-compiled)
cargo build --release -p web-container-tool

# 2. Build the web container contract WASM
cargo build --release --target wasm32-unknown-unknown -p web-container-contract

# 3. Generate test keys
mkdir -p test-contract
target/release/web-container-tool generate --output test-contract/test-keys.toml

# 4. Start an isolated test node (in a separate terminal or background)
#    Choose an appropriate log directory for your OS:
#      macOS:  ~/Library/Logs/freenet-test-node
#      Linux:  ~/.local/share/freenet-test-node/logs
LOG_DIR=~/Library/Logs/freenet-test-node   # adjust for your OS
mkdir -p "$LOG_DIR"
freenet network \
  --network-port 31338 \
  --ws-api-port 7510 \
  --ws-api-address 0.0.0.0 \
  --is-gateway \
  --skip-load-from-network \
  --data-dir ~/freenet-test-node/data \
  --public-network-address 127.0.0.1 \
  --log-dir "$LOG_DIR" \
  --log-level debug
```

### Fast iteration script

```bash
# Full rebuild + republish (~15s):
./scripts/local-republish.sh

# Skip UI build if only repackaging (~2s):
./scripts/local-republish.sh --skip-build

# Target a different port:
./scripts/local-republish.sh --port 7509
```

The script outputs desktop and phone URLs after publishing.

## Build Commands

### Individual components

```bash
cargo build --release --target wasm32-unknown-unknown -p room-contract  # Room contract WASM
cargo build --release --target wasm32-unknown-unknown -p chat-delegate   # Chat delegate WASM
cargo build --release --target wasm32-unknown-unknown -p web-container-contract  # Web container
(cd ui && dx build --release)                                            # UI (Dioxus)
```

### Development mode

```bash
cargo make dev                     # dx serve with hot reload (localhost:8080)
cargo make dev-example             # dx serve with example data (no network needed)
```

## Fast Iteration Loop

### UI changes only (fastest)

```bash
# 1. Make your UI change in ui/src/
# 2. Rebuild + republish:
./scripts/local-republish.sh
# 3. Hard-refresh browser (Cmd+Shift+R / Ctrl+Shift+R)
```

### Contract changes

```bash
# 1. Rebuild contract
cargo build --release --target wasm32-unknown-unknown -p room-contract

# 2. Copy to UI public dir (UI embeds contract WASM)
cp target/wasm32-unknown-unknown/release/room_contract.wasm ui/public/contracts/

# 3. Full rebuild + publish
./scripts/local-republish.sh
```

### Delegate changes

```bash
# 1. Rebuild delegate (UI includes delegate via include_bytes!)
cargo build --release --target wasm32-unknown-unknown -p chat-delegate

# 2. Rebuild UI (picks up new delegate) + publish
./scripts/local-republish.sh
```

## Manual publish

The `cargo make` publish tasks cross-compile the web-container-tool for
`x86_64-unknown-linux-gnu`. On other platforms, use `local-republish.sh`
or run the steps manually:

```bash
# 1. Build UI
(cd ui && dx build --release)

# 2. Compress
(cd target/dx/river-ui/release/web/public && tar -cJf ../../../../../webapp/webapp.tar.xz *)

# 3. Sign with test keys
target/release/web-container-tool sign \
  --input target/webapp/webapp.tar.xz \
  --output target/webapp/webapp-test.metadata \
  --parameters target/webapp/webapp-test.parameters \
  --key-file test-contract/test-keys.toml \
  --version $(( $(date +%s) / 60 ))

# 4. Publish
fdev --port 7510 execute put \
  --code target/wasm32-unknown-unknown/release/web_container_contract.wasm \
  --parameters target/webapp/webapp-test.parameters \
  contract \
  --webapp-archive target/webapp/webapp.tar.xz \
  --webapp-metadata target/webapp/webapp-test.metadata
```

## Debugging

### Debug overlay

Built-in debug overlay activated via `?debug=1` query parameter. Shows
timestamped log messages on-screen with a minimize/expand toggle — essential
for mobile where console is inaccessible.

```
http://{IP}:7510/v1/contract/web/{CONTRACT_ID}/?debug=1
```

Use `crate::util::debug_log("msg")` to log to the overlay. Does nothing
without `?debug=1`.

### Panic overlay

A WASM panic hook creates a visible red error overlay showing the panic
message. Appears automatically on any crash, no query param needed.

### Delegate signing flow

Message sending uses a delegate-based signing architecture:

1. **Room creation** → `create_room_modal.rs` generates `SigningKey`, stores in ROOMS signal, and calls `store_signing_key()` to save it in the chat delegate
2. **Message send** → UI calls `sign_message_with_fallback(room_key, msg, fallback_sk)`
3. **Delegate signing** → `send_delegate_request(SignMessage{...})` → delegate looks up `signing_key:{origin}:{room_key}` → returns signature
4. **Fallback** → If delegate fails, signs locally with `fallback_sk.sign()`
5. **Delta applied** → Message added to local state → `NEEDS_SYNC` set → `ProcessRooms` → UPDATE sent

Key debugging points:
- Node logs show `"Sign request for room, signature created: true/false"` — if false, delegate doesn't have the key
- Browser console shows fallback path: `"Delegate signing failed, using fallback"`
- If no UPDATE appears in node logs after signing, check if WebSocket is still connected

### Check contract state via riverctl

```bash
riverctl --node-url ws://127.0.0.1:7510/v1/contract/command?encodingProtocol=native room list
```

### Timeline analysis for message send

1. `SignMessage received` → delegate got the sign request
2. `signature created: true/false` → delegate had (or didn't have) the key
3. `Update { key: ... }` → UPDATE arrived at node
4. `ResultRouter received result` → UPDATE processed, result sent back to client

If step 1 happens but step 3 doesn't, the browser died between signing and sending the UPDATE.

### Firefox mobile: Dioxus RefCell re-entrant borrow panics

Firefox mobile runs Dioxus signal subscriber notifications synchronously
during Drop, unlike Chrome/Safari which defer to microtask boundaries. This
causes `RefCell already borrowed` panics in WASM at three levels:

1. **Dioxus signal re-entrancy** — `ROOMS.with_mut()` Drop triggers subscriber
   notifications that cascade into `ROOMS.read()`. Fix: use `try_read()` for
   all reactive signal reads. `try_read()` still registers Dioxus subscriptions
   (confirmed in Dioxus 0.7.x source) but returns `Err` instead of panicking.

2. **wasm-bindgen-futures task re-entrancy** — `spawn_local` inside a polled
   future causes re-entrant `Task::run()` at `singlethread.rs:132`. Fix: use
   `safe_spawn_local()` helper (in `util.rs`) that wraps spawn_local in
   `setTimeout(0)` to break out of the WASM call stack.

3. **Signal mutation inside spawn_local** — `ROOMS.with_mut()` inside a
   spawn_local task triggers notifications that re-queue the same task. Fix:
   move signal mutations out of spawn_local via `setTimeout(0)`.

**Key pattern for safe signal writes in WASM:**
```rust
// WRONG — can cause re-entrant borrow in Firefox
spawn_local(async {
    // ... async work ...
    ROOMS.with_mut(|rooms| { /* mutate */ });
});

// RIGHT — defer mutation to clean execution context
#[cfg(target_arch = "wasm32")]
{
    let cb = Closure::once_into_js(move || {
        ROOMS.with_mut(|rooms| { /* mutate */ });
    });
    web_sys::window().unwrap()
        .set_timeout_with_callback(&cb.into()).ok();
}
```

**Important:** Signal clears in `use_effect` must be synchronous, not deferred.
Deferring a clear that the effect subscribes to causes an infinite loop.

See `mark_needs_sync()` in `app.rs` and `safe_spawn_local()` in `util.rs`
for canonical examples.

### Common issues

| Symptom | Cause | Fix |
|---------|-------|-----|
| Messages fail to send (new room) | Signing key not stored in delegate | Fixed: `create_room_modal.rs` now calls `store_signing_key` after room creation |
| "signature created: false" in node logs | Delegate can't find signing key for room | Ensure `StoreSigningKey` is sent after room creation; fallback signs locally |
| Mobile send appears stuck | Browser suspends WASM when screen locks | Keep phone screen active; delegate signing avoids long async chains |
| `RefCell already borrowed` on Firefox mobile | Dioxus signal re-entrant borrow during Drop | Use `try_read()` instead of `read()` for reactive signal access |
| Crash at `singlethread.rs:132` | spawn_local inside polled future on Firefox | Use `safe_spawn_local()` to defer via setTimeout(0) |
| Blank page after code change (no panic) | Infinite loop from deferred signal clear | Keep signal clears synchronous in use_effect; only defer spawns |
