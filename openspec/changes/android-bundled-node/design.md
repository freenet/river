## Context

Phase 1 of the Android port (recorded under "Bundled Freenet node (linked,
not yet started)" in `AGENTS.md`) put every linkage-level prerequisite in
place:

- `vendor/freenet/` (regenerated via `scripts/vendor-freenet.sh`) with the
  Windows/macOS GUI dep blocks stripped so it co-exists with
  `dioxus-desktop`'s `wry 0.53` / `tao 0.34`.
- Workspace `[patch.crates-io]` redirect to the vendored copy.
- `freenet 0.2.61` pulled into `ui/Cargo.toml` under
  `[target.'cfg(target_os = "android")']` only, so the web build is
  unaffected.
- `ui/src/platform.rs` returning `None` from `window()` off wasm and a
  `spawn_local` shim that picks Dioxus's task spawner on native.
- `ui/src/freenet_transport.rs` wrapping the native side of an mpsc
  channel as a stand-in `WebApi` whose `send` signature matches stdlib's
  wasm `WebApi`.
- A native `ConnectionManager` in
  `ui/src/components/app/freenet_api/connection_manager.rs` that uses
  `tokio_tungstenite` to dial `ws://127.0.0.1:7509`.
- `ui/src/node_runtime.rs::start_embedded_node()` that builds a
  `freenet::config::Config` in local mode and calls
  `freenet::run_local_node` on a dedicated tokio runtime.

What's missing is the *runtime* wiring: `start_embedded_node` is never
called (`mod node_runtime;` isn't even declared in `main.rs`), the
native synchronizer module still has unconditional `wasm_bindgen`
imports that would block a sync-enabled native compile, the embedded
node has no contracts in its store, the storage path is hardcoded, and
the OS will reclaim the process when the activity backgrounds. On the
contract side, the room-contract and chat-delegate WASMs in
`ui/public/contracts/` are pre-stdlib-0.8 and would produce a contract
key that no other client on the network recognises.

## Goals / Non-Goals

**Goals:**

- A clean install on a real Android device boots the embedded node,
  serves the bundled contract + delegate from its store, and the UI
  reaches `SynchronizerStatus::Connected` against `ws://127.0.0.1:7509`
  without external infrastructure.
- An invitation URL pasted from web works on Android: GET resolves over
  the bundled node and the room renders.
- The node survives short backgrounding (screen off, home button) for
  long enough that returning to the app does not require a fresh GET.
- The contract namespace converges with River-web: republishing the web
  bundle after this change lands does not strand existing rooms — the
  migration registries cover the key rotation.

**Non-Goals:**

- iOS port. The mobile renderer compiles for iOS, but iOS has no
  in-process embedded node yet and the foreground-service strategy here
  is Android-specific.
- Acting as a Freenet *gateway*. The node runs in `OperationMode::Local`
  only.
- Auth-token handshake. Loopback connections from the bundled UI to the
  bundled node are trusted; the gateway-style token round-trip is
  intentionally not ported.
- Visual or UX changes. This change is entirely about the transport
  layer underneath the existing UI.

## Decisions

### 1. Bootstrap order: node starts before the synchronizer dials

`App()` calls `node_runtime::start_embedded_node()` before the existing
`use_effect` that spins up `FreenetSynchronizer`. The native
`ConnectionManager` already has retry/backoff (`reconnect_delay_ms`), so
a slow node startup is recoverable, but ordering the boot first keeps
the first-connection-success path warm.

*Alternative considered:* lazy-start on first synchronizer connect
attempt. Rejected — couples node lifecycle to UI event timing and makes
the foreground-service handoff in decision 5 harder to reason about.

### 2. Operate the bundled node in Network mode, with gateway bootstrap

`node_runtime.rs::run_node` currently hardcodes `OperationMode::Local`
as a Phase-1 placeholder. We switch it to `OperationMode::Network`
because Local mode only serves what's been PUT to the device:

- A user invited to someone else's room cannot fetch the room state
  (it lives on the network, not the device).
- A user with no rooms has nothing to do until they create one.
- The whole point of Freenet for a chat app is that state is on a
  distributed peer set, not on each device.

Network mode replaces the simple `run_local_node(executor, socket)`
call with the longer `serve_client_api` + `NodeConfig::new` +
`node_config.build` + `run_network_node` sequence from
`freenet/src/bin/freenet.rs::run_network`. The same `WS API` socket
that the UI's native `ConnectionManager` already dials (loopback
`7509`) is served by `serve_client_api`.

Bootstrap: the freenet binary fetches `gateways.toml` from
`https://freenet.org/keys/gateways.toml` on first launch (see
`config.rs::FREENET_GATEWAYS_INDEX`) and writes it under the config
dir. We rely on the same path — the Android app's outbound HTTPS
request to `freenet.org` is the cold-start dependency; everything
after first-launch works against the cached `gateways.toml`.

*Alternative considered:* run Local mode and pre-seed bundled WASMs
into the stores. Rejected as the primary plan because seeding doesn't
buy correctness — River's existing PUT path already carries WASM
bytes, the store dedupes by `code_hash`, and Local mode can't fetch
others' rooms at all. Seeding becomes a P2 perf optimization
(faster first-PUT, slightly less I/O on the JS thread for room
creation) and is deferred to a follow-up change.

*Alternative considered:* gateway-only mode (Android device dials a
gateway as the web build does, no in-process node). Rejected — that
defeats the "bundled node" goal and reintroduces a centralised
infrastructure dependency the project is trying to avoid. Web stays
gateway-based until web has a path off it; Android skips that step.

### 3. Synchronizer compile-fix: cfg-gate, don't shim

The unconditional `use wasm_bindgen::{prelude::Closure, JsCast};` at the
top of `freenet_synchronizer.rs` is dead on native (no use site outside
the `cfg(target_arch = "wasm32")` jitter branch). Gate the imports;
don't introduce a `wasm_bindgen` stub crate. This keeps the
synchronizer source identical for web and native and avoids dragging in
a shim that future contributors would have to keep working.

### 4. Storage path: JNI, not hardcoded constant

`node_runtime.rs` currently hardcodes
`/data/data/org.freenet.river/files/freenet`. Replace with a JNI call
that walks `Context.getFilesDir()` once at startup. The constant happens
to be correct for the published bundle id, but Android can pick a
different `dataDir` under multi-user / work-profile configurations, and
a hard path would silently send writes to a non-existent location.

### 5. Foreground service for node lifetime

Wrap the embedded node's tokio runtime inside an Android foreground
service started from `MainActivity.onCreate()` (declared in the
`ui/Dioxus.toml` `[android.permissions]` block and the generated
manifest). The service holds an ongoing notification; Android then
treats it as non-killable while the user is using other apps. Stopped
from `MainActivity.onDestroy()`.

*Alternative considered:* `WorkManager` periodic job. Rejected —
periodic jobs are minutes-granularity and would let messages miss a
sync window. A foreground service is the documented Android answer for
"long-running work with user awareness."

### 6. Contract rebuild + migration registry update

Rebuild `room_contract.wasm` + `chat_delegate.wasm` against
`freenet-stdlib 0.8` via `cargo make sync-wasm`. Run
`cargo make add-migration` BEFORE the rebuild so the OLD hash gets
captured into `legacy_delegates.toml`; run
`cargo make add-room-contract-migration` for the room-contract side.
Then republish River-web in lockstep with the first Android release so
the public network has a homogeneous contract pair.

## Risks / Trade-offs

- [Storage path JNI lookup fails on emulator / non-Activity context] →
  Fall back to the hardcoded `/data/data/org.freenet.river/files/freenet`
  constant and log a `warn!`. Worst case is the emulator can't write
  outside Activity scope, which is already broken today.

- [Mobile NAT (carrier-grade, symmetric) blocks Freenet peer
  connections] → The biggest unknown. Freenet's transport uses UDP with
  STUN-style hole punching; symmetric NAT (common on cellular) defeats
  it. Mitigation: rely on gateways to relay until peer connections
  establish, document Wi-Fi-only as the supported path for the first
  release, and instrument connection-success telemetry per network type
  so we can size the problem from real data.

- [Battery / cellular data usage from holding peer connections] →
  Phase-2 mitigations are coarse: don't enable Network mode while on
  battery saver; back off when the screen is off; respect Android's
  doze-mode signals. Phase-3 work would tune the LEDBAT congestion
  controller for mobile RTT / packet-loss profiles.

- [`gateways.toml` cold-start fetch fails (offline first launch, blocked
  freenet.org domain)] → Bundle a fallback `gateways.toml` snapshot as
  an APK asset, used when the live fetch fails. Update on each release.
  Worst case: a stale gateway is one extra round-trip before discovery
  finds a fresher peer.

- [Foreground service notification surprises users] →
  Use a low-priority notification with copy that mentions River is
  serving rooms locally. Document the notification in the Play Store
  listing.

- [stdlib-0.8 contract WASM is incompatible with the host runtime] →
  The Android-bundled `freenet 0.2.61` is stdlib-0.8 compatible by
  construction (it's why we vendored 0.2.61 specifically). The risk is
  on the *web* side: republishing River-web against new WASMs needs the
  workspace `freenet-stdlib = "0.8"` pin to flow through to the
  contract crates as well. The contract crates already build green
  against this pin (their `cargo make test-*` targets pass), so this is
  a verification step, not a code change.

- [Migration registry races a partial deploy] →
  Land all four pieces together: WASM rebuilds, both migration
  registries updated, web republish, Android release. The Phase 1 work
  already changed the workspace stdlib pin in a vacuum without a
  contract rebuild, which is why a coordinated republish is the right
  unit of change.

## Migration Plan

1. Land the code changes (decisions 1–5) under a `no-sync` build first
   so the Phase 1 build path keeps passing CI through the PR review.
2. Rebuild contract WASMs (decision 6); land the migration registry
   entries in the same commit as the new WASMs to satisfy the
   `check-delegate-migration` / `check-room-contract-migration` workflows.
3. Republish River-web from main and verify against the existing curl
   smoke test (`AGENTS.md` "Verify deployment").
4. Cut a tagged Android build, side-load on a test device, walk through
   the acceptance scenarios in `specs/android-bundled-node/spec.md`.
5. Rollback: revert the four pieces as a unit. Because the migration
   registries are additive, a rollback that re-publishes the OLD web
   contract pair simply leaves the new entries unused — no data is lost
   on the network side. Android users on the new release would see GET
   failures against the old contract namespace; the rollback would also
   pull the Android release from the Play Store track.

## Open Questions

- Do we ship the foreground-service notification on *every* Android
  launch, or only when at least one room is synced? The latter is more
  polite but adds a "service stopped" / "service started" handshake
  whenever the room set transitions between empty and non-empty.
  Defaulting to "always while app process alive" for the first cut.
- What's the right `consecutive_failures` cap on the native
  `ConnectionManager`? The wasm path benefits from page reload as a
  hard reset; on Android there's no equivalent. Currently leaning on
  the existing exponential-backoff loop and a manual "reset connection"
  affordance in a follow-up.
