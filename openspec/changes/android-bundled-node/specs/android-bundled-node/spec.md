## ADDED Requirements

### Requirement: Embedded Freenet node boots on Android launch

On Android, the River process SHALL start an in-process Freenet node in
`OperationMode::Local` before the UI synchronizer attempts its first
WebSocket connection. The node SHALL bind its WebSocket API on
loopback at the freenet-default port (7509 in local mode).

#### Scenario: Cold launch on a clean install

- **WHEN** the user installs the River APK and opens it for the first time
- **THEN** within 10 seconds of the launcher activity becoming visible, a
  process listening on `127.0.0.1:7509` SHALL be running inside the
  River process, AND the synchronizer status visible in the UI SHALL
  transition through `Connecting` to `Connected`

#### Scenario: Warm launch after force-stop

- **WHEN** the user force-stops River and reopens it
- **THEN** the embedded node SHALL come back up using the existing
  storage dir without re-running the WASM-seeding step from the
  next requirement

### Requirement: Embedded node runs in Network mode with gateway bootstrap

The embedded node SHALL run in `OperationMode::Network`, NOT in
`OperationMode::Local`. On first launch the app SHALL acquire a
`gateways.toml` file (fetched from
`https://freenet.org/keys/gateways.toml` if reachable, otherwise
loaded from a fallback snapshot bundled as an Android asset) and
write it to the node's config dir. On subsequent launches the cached
`gateways.toml` SHALL be reused.

#### Scenario: Network-mode boot resolves a known contract from peers

- **WHEN** the user accepts an invitation URL for a room they have
  never seen before
- **THEN** the embedded node SHALL fetch the room contract WASM and
  current state through Freenet peer connections, AND the room SHALL
  render on the device — NOT fail with `contract not found`

#### Scenario: First-launch gateway fetch caches to disk

- **WHEN** River is opened for the first time AND `freenet.org` is
  reachable
- **THEN** the embedded node SHALL persist a `gateways.toml` file
  under its config dir, AND a subsequent launch with `freenet.org`
  blocked SHALL still come up using the cached file

#### Scenario: First-launch fallback when gateway fetch fails

- **WHEN** River is opened for the first time AND the
  `https://freenet.org/keys/gateways.toml` request fails (network
  blocked, DNS denied)
- **THEN** the embedded node SHALL load a bundled fallback
  `gateways.toml` from the APK assets AND continue startup, instead
  of aborting

### Requirement: Bundled WASM hashes match the committed web bundle

The room-contract and chat-delegate WASMs linked into the APK SHALL
have BLAKE3 code hashes that match the same files committed under
`ui/public/contracts/`. This guarantees Android clients publish to
and resolve from the same contract / delegate keys as web clients.

#### Scenario: Bundled WASM hash matches the live web bundle

- **WHEN** the Android build is produced from a commit on `main`
- **THEN** the BLAKE3 hash of `room_contract.wasm` AND of
  `chat_delegate.wasm` linked into the APK SHALL each equal the hash
  of the corresponding file committed at `ui/public/contracts/`

### Requirement: Synchronizer compiles and runs against the embedded node

The `FreenetSynchronizer` module SHALL compile under
`cfg(not(target_arch = "wasm32"))` AND under
`cfg(target_arch = "wasm32")` from the same source. On Android, the
synchronizer SHALL drive the same room / message lifecycle it drives
in the web build, going through the native `ConnectionManager` and
the embedded node.

#### Scenario: Native cargo check passes without no-sync

- **WHEN** `cargo check -p river-ui --target aarch64-linux-android`
  is run without enabling the `no-sync` feature
- **THEN** the build SHALL succeed AND the
  `FreenetSynchronizer` symbols SHALL be reachable from
  `App()`'s startup path

#### Scenario: Sending a message round-trips through the embedded node

- **WHEN** a user joins a room on Android, types a message, and
  submits
- **THEN** the message SHALL be observable in the synchronizer's
  outbound queue, the embedded node SHALL accept the resulting UPDATE
  delta, AND the message SHALL render in the room's message list
  without a refresh

### Requirement: Storage path is resolved from the Android runtime

River SHALL resolve the embedded node's storage directory at startup
by calling `Context.getFilesDir()` via JNI, NOT by referencing a
compile-time string constant. If the JNI call fails (e.g. running on
an emulator with no attached Activity), River SHALL fall back to the
known package-private path and log a warning.

#### Scenario: Storage path matches Android-assigned location

- **WHEN** the embedded node creates its data directory at startup on
  a real device
- **THEN** the resulting path SHALL be inside the same directory tree
  returned by `Context.getFilesDir()` for the running process

#### Scenario: JNI lookup failure falls back gracefully

- **WHEN** the JNI lookup for `getFilesDir()` fails
- **THEN** River SHALL fall back to a documented package-private path,
  emit a `warn!` log entry, AND continue with node startup rather than
  aborting the process

### Requirement: Embedded node lifetime is owned by an Android foreground service

The embedded node SHALL run inside an Android foreground service
declared in the app's manifest with the `INTERNET` and
`FOREGROUND_SERVICE` permissions. The service SHALL post an ongoing
notification while running. Stopping the foreground service SHALL
also signal the embedded node's tokio runtime to shut down cleanly.

#### Scenario: Backgrounding the activity keeps the node alive

- **WHEN** the user opens River, joins a room, then sends River to the
  background by pressing the home button
- **THEN** the embedded node process SHALL remain listening on
  `127.0.0.1:7509` for at least 30 seconds AND, on returning to the
  app, the synchronizer SHALL still report `Connected` without
  reconnecting from scratch

#### Scenario: Service notification is present while node runs

- **WHEN** the embedded node is running
- **THEN** an ongoing notification associated with River's foreground
  service SHALL be visible in the Android status bar

#### Scenario: Service stop cleans up the node

- **WHEN** the user swipes River away from the recents list AND the
  Android OS stops the foreground service
- **THEN** the tokio runtime hosting the embedded node SHALL receive a
  shutdown signal AND release its bound loopback port within 5 seconds

### Requirement: Contract and delegate namespaces converge with the web build

River SHALL build the room-contract and chat-delegate WASMs bundled
with the Android app against `freenet-stdlib 0.8`, and the previous
generation's BLAKE3 code hashes SHALL be recorded in
`legacy_delegates.toml` and `common/legacy_room_contracts.toml` so
that pre-existing rooms migrate forward on first GET. The same WASMs
SHALL drive a coordinated River-web republish in the same release.

#### Scenario: Legacy delegate hash is registered before publish

- **WHEN** the Android release is cut and a commit on `main` updates
  the bytes of `ui/public/contracts/chat_delegate.wasm`
- **THEN** the previous BLAKE3 code hash of that file SHALL be present
  as an entry in `legacy_delegates.toml`, AND
  `cargo make check-migration` SHALL exit zero

#### Scenario: Legacy room-contract hash is registered before publish

- **WHEN** the Android release is cut and a commit on `main` updates
  the bytes of `ui/public/contracts/room_contract.wasm`
- **THEN** the previous BLAKE3 code hash of that file SHALL be present
  as an entry in `common/legacy_room_contracts.toml`, AND the
  `check-room-contract-migration` CI workflow SHALL pass

#### Scenario: Existing web room recovers after the coordinated republish

- **WHEN** a user with rooms in the pre-stdlib-0.8 namespace loads the
  republished River-web build
- **THEN** the legacy room-contract probe loop in
  `common/src/migration.rs` SHALL locate the user's room under the
  old generation, AND the room state SHALL be re-PUT under the new
  contract key without user action
