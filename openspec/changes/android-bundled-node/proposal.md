## Why

River's Android target currently builds and launches as a no-sync local
client: `cargo make build-android` produces an APK, the WASM-only branches
in `ui/src/platform.rs` are skipped on native, and `ui/src/node_runtime.rs`
exposes a `start_embedded_node()` entrypoint that compiles. The remaining
ingredients to make Android a *functional* River peer (boot the node, seed
its stores with the bundled contract/delegate WASMs, complete the
synchronizer's native compile, and resolve the stdlib-0.8 contract
namespace split) are deferred per the "What's still deferred" list in
`AGENTS.md`. This change closes that gap so a fresh APK install can join
a public room over the in-device Freenet node and exchange messages with
existing web peers.

## What Changes

- Start the embedded Freenet node from `App()` on Android — wire
  `node_runtime::start_embedded_node()` (currently unused; the module is
  not even declared in `ui/src/main.rs`) into the app's startup path,
  before the synchronizer dials loopback. **(done; see commit
  `a40b9ca9`)**
- Make the synchronizer compile on native — replace the unconditional
  `wasm_bindgen::{Closure, JsCast}` imports in
  `freenet_api/freenet_synchronizer.rs` with `cfg`-gated imports, and
  drop `feature = "no-sync"` from `cargo make build-android` /
  `serve-android` so the native `ConnectionManager` (already implemented
  in `connection_manager.rs`) actually runs. **(done; see commit
  `a40b9ca9`)**
- Switch the bundled node from `OperationMode::Local` to
  `OperationMode::Network`. Local mode only serves what's been PUT to
  it locally — Android users could create their own rooms but not join
  any room shared via invitation link, because the network state never
  reaches their device. Network mode makes the device a real Freenet
  peer that fetches contracts and states through the network.
- Configure the bundled node with a gateway list. On first launch the
  app fetches `gateways.toml` from `https://freenet.org/keys/gateways.toml`
  (the same default the freenet binary uses) and caches it under the
  app data dir, so subsequent launches work offline-to-bootstrap.
- Resolve the storage path via JNI rather than the hardcoded
  `/data/data/org.freenet.river/files/freenet` constant in
  `node_runtime.rs`.
- Run the bundled node inside an Android foreground service so the OS
  does not kill it when the activity backgrounds, and declare
  `INTERNET` + `FOREGROUND_SERVICE` (with the correct `dataSync` /
  `connectedDevice` foreground service type for Android 14+) in the
  generated manifest.
- Rebuild `room_contract.wasm` and `chat_delegate.wasm` against
  `freenet-stdlib 0.8`, append the old hashes to `legacy_delegates.toml`
  and `common/legacy_room_contracts.toml`, run `cargo make sync-wasm`,
  and republish River-web so Android and web share a contract namespace.
- **BREAKING** for already-installed web users: republishing rotates the
  delegate key and the room-contract key. Mitigated by the migration
  registries above so existing rooms migrate forward on first GET.

### Deferred (post-MVP, not in this change)

- **Pre-seeding the bundled WASMs into the node's `contract_store` /
  `delegate_store`.** Originally proposed as a correctness requirement,
  but investigation found it is a pure perf optimization: River's
  existing PUT path (room creation, delegate registration) carries the
  WASM bytes and the local node's store dedupes by `code_hash`. The
  first PUT is marginally slower without seeding; everything still
  works. Deferred to a follow-up change so it does not block the MVP.

## Capabilities

### New Capabilities

- `android-bundled-node`: River boots an in-process Freenet node on
  Android, seeds it with bundled contract/delegate WASMs, and the UI
  syncs against it over loopback for the lifetime of the app process.

### Modified Capabilities

- None at requirement level. The web build remains unchanged in
  behaviour; only WASM hashes shift, which the existing migration
  machinery covers.

## Impact

- **Code**: `ui/src/main.rs` (module decl + startup hook), `ui/src/components/app.rs`
  (call `start_embedded_node` from `App()`), `ui/src/node_runtime.rs`
  (switch to Network mode, gateway bootstrap, storage path,
  service-lifecycle hooks), `ui/src/components/app/freenet_api/freenet_synchronizer.rs`
  (cfg-gated imports), `ui/Cargo.toml` (Android `jni` dep),
  `Makefile.toml` (drop `no-sync`), `ui/Dioxus.toml` (Android manifest
  entries for permissions + foreground service).
- **Build artifacts**: `ui/public/contracts/room_contract.wasm` and
  `chat_delegate.wasm` get rebuilt against stdlib 0.8; their previous
  hashes are recorded in `legacy_delegates.toml` and
  `common/legacy_room_contracts.toml`.
- **Deploy**: One coordinated River-web republish so the network has a
  matching contract pair when the first Android client lands.
- **Dependencies**: `freenet 0.2.61` is already added under
  `cfg(target_os = "android")`. Adds `jni` for the storage-path query
  and `ndk-context` for the foreground-service binding.
- **Network**: Android devices become Freenet peers on cellular and
  Wi-Fi. Battery, data-usage, and NAT-traversal behavior on mobile
  networks (carrier-grade NAT is often symmetric) are new exposure.
