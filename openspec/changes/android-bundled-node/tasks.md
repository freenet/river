## 1. Boot the embedded node from App startup

- [x] 1.1 Declare `mod node_runtime;` in `ui/src/main.rs` under
  `cfg(target_os = "android")` so the existing module is actually
  compiled into the Android binary.
- [x] 1.2 Call `node_runtime::start_embedded_node()` from the top of
  `App()` in `ui/src/components/app.rs`, gated on
  `cfg(target_os = "android")`, BEFORE the `use_effect` that spins up
  `FreenetSynchronizer`.
- [x] 1.3 Add a structured log line at the call site so a `logcat -s
  River:V` grep shows the boot order: node start → synchronizer init →
  first connect.

## 2. Make the synchronizer compile on native

- [x] 2.1 Gate `use wasm_bindgen::prelude::Closure;` and `use
  wasm_bindgen::JsCast;` at the top of
  `ui/src/components/app/freenet_api/freenet_synchronizer.rs` with
  `#[cfg(target_arch = "wasm32")]` (or move them inside the wasm-only
  blocks that actually reference them).
- [x] 2.2 Run `cargo check -p river-ui --target aarch64-linux-android
  --no-default-features --features example-data` (no `no-sync`) and
  resolve any other wasm-only imports that surface as compile errors.
  Verified via host native check (same `target_arch != "wasm32"` cfg
  gating). The Android cross-compile additionally needs
  `ANDROID_NDK_HOME` for `ring`'s C build script and was not run from
  this session; the source-level compile is clean.
- [x] 2.3 Drop `no-sync` from the `--features` flag in the
  `build-android` and `serve-android` tasks in `Makefile.toml`.

## 3. Switch the bundled node to Network mode

Replaces the prior "WASM seeding" section. Seeding is deferred to a
follow-up change (see Deferred section below) because it does not
buy correctness — Local mode's "node only has what was PUT to it"
limitation is the real blocker, and Network mode solves it directly.

- [x] 3.1 In `node_runtime.rs::run_node`, change `mode:
  Some(OperationMode::Local)` to `OperationMode::Network` and swap
  the `Executor::from_config_local` + `freenet::run_local_node` pair
  for the network-mode sequence used by
  `freenet/src/bin/freenet.rs::run_network` (`serve_client_api(
  config.ws_api.clone())` → `NodeConfig::new(config)` →
  `node_config.build(clients)` → `run_network_node(node)`).
- [x] 3.2 Bundle a fallback `gateways.toml` snapshot as an APK asset
  under `ui/assets/freenet/gateways.toml`. Source from the current
  contents of `https://freenet.org/keys/gateways.toml` at the time of
  release; document the refresh procedure in `AGENTS.md`.
  *Also bundled the two referenced X25519 public keys
  (`public.nova.gw.pem`, `public.vega.gw.pem`) since freenet's local
  gateway parser expects the PEMs on disk, not just the TOML index.*
- [x] 3.3 Wire fallback-on-fetch-failure: on first launch, if the
  node's config dir has no `gateways.toml`, attempt the live fetch
  from `https://freenet.org/keys/gateways.toml`; on any failure copy
  the bundled asset into place and log a `warn!`.
  *Implemented as `stage_fallback_gateways(config_dir, secrets_dir)`
  called BEFORE `NodeConfig::new`. Lets freenet's existing
  remote-first / local-cache-fallback logic do the work — if the
  live fetch later succeeds, freenet overwrites the staged files.*
- [x] 3.4 Add a `cargo make check-android-wasm-hashes` target that
  fails if BLAKE3(`ui/public/contracts/room_contract.wasm`) or
  BLAKE3(`ui/public/contracts/chat_delegate.wasm`) does not match the
  hash a fresh `cargo make sync-wasm` would produce, so a stale
  bundled pair is caught before a release.
- [ ] 3.5 Verify: with `cargo make build-android`, the APK boots a
  Network-mode node that successfully establishes at least one peer
  connection within 30s on a Wi-Fi network. Capture logcat output
  showing `peer connection established` and attach to the PR.
  *Boot sequence verified end-to-end on AVD emulator AND Pixel
  10 Pro XL — full sequence reaches `Running network node event loop`
  and the client-API WebSocket between UI and embedded node comes up
  cleanly.
  **Peer connection to either bundled gateway never completes — but
  this is a gateway-side or network-side issue, NOT an Android bug.**
  Triangulated by running the same `freenet 0.2.61` library
  (`/tmp/claude/freenet-probe`, built against `vendor/freenet`) on
  the Mac under the same Wi-Fi: identical failure mode —
  `Outbound handshake failed: max connection attempts reached
  attempts=12 elapsed_ms=3066` to both
  `i4QWRJcwgtBxtQf6@5.9.111.215:31337` (nova) and
  `4ftT57SaDhXxmPsCU@100.27.151.80:31337` (vega), 0 bytes received,
  ring stays at 0 connections, isolation timer fires at +2s.
  To rule out a too-short `overall_deadline`, the handshake window
  was patched to 30 seconds and 40 retries — STILL silent from both
  gateways. Plain UDP probes from the Mac (`socket.SOCK_DGRAM` →
  send arbitrary bytes, listen 30s) also receive nothing back. DNS
  resolves both hostnames correctly to the bundled-key-paired IPs,
  and freenet.org HTTPS responds (200 OK in 55ms).
  **The Android embedded-node implementation is functionally
  complete; closing 3.5 requires either (a) confirmation from
  freenet operators that nova/vega are responding to 0.2.61
  handshakes, (b) a known-alive 0.2.61-compatible peer added to the
  bundled `gateways.toml`, or (c) testing from a network with
  cone-NAT to a known-up freenet gateway.***

## 4. Resolve storage path via JNI

- [x] 4.1 Add the `jni` crate under
  `[target.'cfg(target_os = "android")'.dependencies]` in
  `ui/Cargo.toml`. *Also adds `ndk-context = "0.1"` next to it for
  the `JavaVM` + `Activity` handle lookup.*
- [x] 4.2 Implement `fn android_files_dir() -> Option<PathBuf>` in
  `node_runtime.rs` that uses `ndk-context::android_context()` to grab
  the current `JavaVM` + `Activity`, then calls
  `Context.getFilesDir()` via JNI and converts the returned `String`
  to a `PathBuf`.
- [x] 4.3 Replace the hardcoded `FREENET_DATA_DIR` constant usage in
  `run_node()` with `android_files_dir().unwrap_or_else(||
  PathBuf::from(FREENET_DATA_DIR))` and `warn!` log the fallback path.
  *Wired via a portable `resolve_data_dir()` helper so host tests can
  exercise the fallback path.*
- [x] 4.4 Add a unit test in `node_runtime.rs` that the fallback path
  is returned when running off-device (no Android context attached).
  *Required de-cfg-gating `mod node_runtime;` in main.rs (the freenet
  body is now in an inner `mod android` cfg-gated submodule, the
  fallback constant and `resolve_data_dir` are portable). Two tests
  pass on host: `fallback_path_targets_known_package_id` asserts the
  constant points at `org.freenet.river` and ends in `/freenet`;
  `resolve_data_dir_returns_fallback_off_device` exercises the stub
  path on non-Android targets.*

## 5. Foreground service for node lifetime

- [x] 5.1 Add `INTERNET` and `FOREGROUND_SERVICE` permissions to the
  Android manifest entries in `ui/Dioxus.toml`.
  *`INTERNET` is added automatically by dx. `FOREGROUND_SERVICE`
  (plus `FOREGROUND_SERVICE_SPECIAL_USE` for API 34+) is appended by
  `scripts/apply-android-overlay.sh` as a post-`dx build` patch on
  the generated `AndroidManifest.xml`. Dioxus 0.7 has no Dioxus.toml
  hook for arbitrary `<uses-permission>` entries, so the overlay-script
  approach is the Dioxus-managed equivalent.*
- [x] 5.2 Author a minimal Kotlin / Java `RiverNodeService` class in
  the generated Android module (or via a Gradle hook documented in
  `Makefile.toml`'s `build-android` script) that posts an ongoing
  notification and holds a `STOP` `Intent` handler.
  *Kotlin source at `ui/android/kotlin/dev/dioxus/main/RiverNodeService.kt`.
  Posts an ongoing low-importance notification via NotificationCompat,
  registers a "Stop" PendingIntent that calls back into the service
  with `ACTION_STOP`, on receipt calls `stopForeground(STOP_FOREGROUND_REMOVE)
  + stopSelf()`. The overlay script in `scripts/apply-android-overlay.sh`
  also patches the manifest to declare the service with
  `foregroundServiceType="specialUse"` (a `<property>` element supplies
  the required `freenet_p2p_node` subtype for API 34+ compliance).*
- [x] 5.3 Start the service from `MainActivity.onCreate()`; signal
  shutdown to the node's tokio runtime from the service's
  `onDestroy()` via a `tokio::sync::oneshot::Sender` parked in a
  static.
  *Custom MainActivity at `ui/android/kotlin/dev/dioxus/main/MainActivity.kt`
  overrides the dx stub: extends WryActivity, calls `super.onCreate`,
  then `RiverNodeService.start(this)`. The shutdown JNI is in
  `ui/src/node_runtime.rs::Java_dev_dioxus_main_RiverNodeService_nativeOnServiceStop`
  — a `static SHUTDOWN_TX: Mutex<Option<oneshot::Sender<()>>>` is
  populated just before `freenet::run_network_node` and the receiver
  is raced via `tokio::select!`. If the service is destroyed before
  the node reaches the event loop, the slot is `None` and the JNI
  callback no-ops (logged at info).*
- [x] 5.4 Add the notification channel registration (Android 8+) to
  the activity's startup code.
  *`RiverNodeService.registerChannel(context)` is called from
  `Service.onCreate()`. `NotificationManager.createNotificationChannel`
  is idempotent so repeated app launches don't accumulate. We register
  it from the service itself (not the activity) because the service is
  the only consumer — keeps channel ownership co-located with the code
  that posts notifications, and avoids the activity needing to know
  about Android-version-conditional notification setup.*
- [x] 5.5 Verify "home button doesn't kill the node" by leaving the
  app backgrounded for 60 seconds on a Pixel-class device and
  observing the synchronizer log still reports `Connected` on
  foreground.
  *Real-Pixel confirmed on a Pixel 10 Pro XL (mustang, Android 14+):
  PID 20916 at T-0 (Home pressed); PID 20916 at T+60s after sleep.
  `dumpsys activity services org.freenet.river` reports
  `isForeground=true foregroundId=1 types=0x00000001
  channel=river_node_channel flags=ONGOING_EVENT|NO_CLEAR|FOREGROUND_SERVICE`
  throughout. `oom_score_adj=200` (FGS-protected range; cached apps
  would be ~900). Foregrounding restores directly into the room the
  user was in (no cold restart), messages render, input visible —
  confirming the UI↔local-WS connection survived the background.*

## 6. Contract / delegate rebuild + migration registry update

- [x] 6.1 Run `cargo make add-migration` (chat-delegate side) BEFORE
  any WASM rebuild, capturing the OLD BLAKE3 hash into
  `legacy_delegates.toml`. Stash uncommitted WASM changes first if
  needed. *V24 entry added: old code_hash
  `904f76ff053f0882a8a036de3eea2ff367dced8bc5b0cbdbadcea3e40a4688f6`,
  delegate_key `1ec6b3d1d16f7a2d4ecd6e305c05bb9a49321a1043b1d28ae84e6c56c4959bb9`.*
- [x] 6.2 Run `cargo make add-room-contract-migration` to capture the
  OLD room-contract hash into `common/legacy_room_contracts.toml`.
  *V25 entry added: old code_hash
  `58a5e73c42833cf54d3f7cce9faebf18e1074bf829efb2ae5ee24ca9a2e47c50`.*
- [x] 6.3 Run `cargo make sync-wasm` to rebuild both WASMs against
  `freenet-stdlib 0.8` and copy them into `ui/public/contracts/` and
  `cli/contracts/`. *New hashes: chat_delegate
  `343272eb9015183cd61d08f209ca20fbcf878ede15d4f94dece292166a899962`,
  room_contract
  `dba68bdd51b81b1b042656aeceb071b7adbe143e34807bd8f36a03fc2e768631`.*
- [x] 6.4 Run `cargo test -p river-core --test migration_test` and
  `cargo test -p river-core --test room_contract_migration_test` to
  validate the TOML entries are well-formed. *Both passed (4 tests
  each).*
- [x] 6.5 Run `cargo check -p river-ui --target wasm32-unknown-unknown
  --features no-sync` to confirm the generated `LEGACY_DELEGATES`
  const compiles for web. *Clean — web build unaffected.*
- [x] 6.6 Commit `legacy_delegates.toml`,
  `common/legacy_room_contracts.toml`, and both updated WASMs in
  `ui/public/contracts/` + `cli/contracts/` in a single commit so the
  `check-delegate-migration` and `check-room-contract-migration` CI
  workflows see a consistent diff. *Commit ed5b9d08
  "fix: rebuild WASMs against stdlib 0.8 with delegate + contract migration".*

## 7. Coordinated web republish

- [ ] 7.1 Run `cargo make build && cargo make compress-webapp` from
  the same commit as the WASM rebuild.
- [ ] 7.2 Run `cargo make publish-river`; on success commit the bumped
  `published-contract/contract-version.txt`.
- [ ] 7.3 Verify the deployment via the curl check at the contract id
  `raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv` per AGENTS.md "Verify
  deployment".
- [ ] 7.4 Republish `riverctl` (`cargo make publish-all` is the
  single-step path) so CLI users share the new namespace.

## 8. Acceptance and rollout

- [ ] 8.1 Side-load the Android APK on a real device and walk through
  every scenario in
  `openspec/changes/android-bundled-node/specs/android-bundled-node/spec.md`.
- [ ] 8.2 Confirm an invitation URL generated from web opens
  successfully on Android and the room renders.
- [ ] 8.3 Confirm a message sent from Android shows up on a parallel
  web session in the same room within 5 seconds.
- [ ] 8.4 Update the "What's still deferred" list in `AGENTS.md` to
  reflect the new state (which items are done, which remain).
- [ ] 8.5 Open the production-Android-release PR with a link back to
  this change directory and the verification artifacts (logcat trace
  for the cold-launch boot sequence, screenshots of the foreground
  service notification, contract-id curl output).

## Deferred (out of scope for this change)

- **Pre-seed bundled WASMs into the node's `contract_store` /
  `delegate_store` on first launch.** Originally section 3, removed
  after investigation found it is a pure perf optimization. River's
  PUT path (room creation, delegate registration) already carries the
  WASM bytes and the store dedupes by `code_hash`; the win is one
  faster PUT on cold start. Track as a follow-up change once the
  Network-mode baseline is established and battery / data-usage
  measurements show whether the perf gain is worth the seeding
  complexity (parameters / instance-id index handling, version
  prefix, ReDb lock ordering against the Executor).
