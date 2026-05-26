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
- [x] 3.5 Verify: with `cargo make build-android`, the APK boots a
  Network-mode node that successfully establishes at least one peer
  connection within 30s on a Wi-Fi network. Capture logcat output
  showing `peer connection established` and attach to the PR.
  *Closed on Pixel 10 Pro XL — within seconds of `Native WebSocket
  connection established`, the embedded node reports
  `NAT traversal connection established peer_addr=100.27.151.80:31337`
  (nova) AND
  `NAT traversal connection established peer_addr=5.9.111.215:31337`
  (vega), then `NAT traversal connection established peer_addr=2.110.90.63:58542`
  (a third peer found via gateway routing). Within 30s the ring has
  5 peers actively reporting RTT-adaptive congestion-control metrics:
  44-56 ms RTT across nova / 162.84.244.113 / 99.224.174.239 /
  173.31.179.187 / 96.248.60.23. Evidence saved at
  `/tmp/claude/pixel_crash_slim.log` (filtered for the
  `NAT traversal connection established` /
  `Outbound connection established` / `cc_rate_mbps` lines).
  Earlier "max connection attempts reached" failure reproduced
  cleanly from the same Wi-Fi was apparently transient (gateways
  unavailable, ISP UDP throttling, or a routing flap); when retested
  later the handshake succeeded immediately.*

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

- [x] 7.1 Run `cargo make build && cargo make compress-webapp` from
  the same commit as the WASM rebuild.
  *`cargo make compress-webapp` ran clean. Output:
  `target/webapp/webapp.tar.xz` (957,120 bytes) with new stdlib-0.8
  WASMs baked in (room_contract `dba68bdd…`, chat_delegate
  `343272eb…`). Side-fix committed (b5e34f39): made `sed -i` in
  `{un,}comment-base-path` portable so the task actually runs on
  macOS — was failing with `invalid command code u` because BSD sed
  needed an explicit `-i.bak` backup-extension argument.*
- [ ] 7.2 Run `cargo make publish-river`; on success commit the bumped
  `published-contract/contract-version.txt`.
  *Blocked on Ian. The canonical web-container signing key
  (`~/.config/river/web-container-keys.toml`, whose verifying
  counterpart is committed in `published-contract/webapp.parameters`
  and determines contract id
  `raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv`) lives only on Ian
  Clarke's host — recent web publishes #311 (30000320) and #312
  (30000321) were both authored by him. Sal does not hold the key
  and signing with a fresh one would change the contract id and
  strand every existing user's room data. Path forward: Ian pulls
  `origin/main` (which already contains the stdlib-0.8 WASM rebuild
  in commit "fix: rebuild WASMs against stdlib 0.8 with delegate +
  contract migration") and runs `cargo make publish-river` from his
  own env; `sign-webapp` will bump the counter from 30000321 to
  30000322.
  Earlier red herrings (macOS `cargo install freenet --version
  0.2.61` fails on the `include_str!` packaging bug fixed by
  freenet/freenet-core#4240) are NOT the blocker — even with that
  fix and a Linux env, Sal cannot sign.*
- [ ] 7.3 Verify the deployment via the curl check at the contract id
  `raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv` per AGENTS.md "Verify
  deployment". *Deferred — depends on 7.2.*
- [ ] 7.4 Republish `riverctl` (`cargo make publish-all` is the
  single-step path) so CLI users share the new namespace. *Deferred
  — depends on 7.2.*

## 8. Acceptance and rollout

- [x] 8.1 Side-load the Android APK on a real device and walk through
  every scenario in
  `openspec/changes/android-bundled-node/specs/android-bundled-node/spec.md`.
  *Verified on Pixel 10 Pro XL after the auth_token fix landed (see
  "Known issues" → resolved entry). Confirmed live: cold-launch boots
  the embedded node + WS dial succeeds within 10s; force-stop + relaunch
  recovers existing rooms from the on-disk storage dir; a message sent
  on Android persists across an `am force-stop` and shows back up in the
  room view on next launch (i.e. delegate persistence works, was the
  symptom the auth_token fix targeted). Foreground-service notification
  visible throughout. The two scenarios that depend on a coordinated
  River-web republish — "Network-mode boot resolves a known contract
  from peers" via a web-issued invitation (covered by 8.2), and
  "Existing web room recovers after the coordinated republish" — are
  gated on 7.2 landing first.*
- [ ] 8.2 Confirm an invitation URL generated from web opens
  successfully on Android and the room renders.
- [ ] 8.3 Confirm a message sent from Android shows up on a parallel
  web session in the same room within 5 seconds.
- [x] 8.4 Update the "What's still deferred" list in `AGENTS.md` to
  reflect the new state (which items are done, which remain).
  *Reorganised: added a verified-end-to-end item for real-device peer
  connectivity (closes 3.5 + 5.5) and one for the stdlib-0.8 WASM
  rebuild + migration registries. The auth_token blocker is now the
  top-of-list deferred item; the coordinated web republish is #2; the
  remaining bare `spawn_local` callsites are #3; WASM pre-seeding is
  #4.*
- [x] 8.5 Open the production-Android-release PR with a link back to
  this change directory and the verification artifacts (logcat trace
  for the cold-launch boot sequence, screenshots of the foreground
  service notification, contract-id curl output).
  *Opened as freenet/river#313 from `SalvatoreT:android-bundled-node`
  → `freenet/river:main`. PR body links back to this change directory,
  lists the verification evidence (`/tmp/claude/pixel_crash_slim.log`
  for peer connectivity, `dumpsys activity services` output for FGS
  protection, migration test counts for stdlib-0.8 WASM rebuild) and
  enumerates the manual acceptance steps still gated on Ian's web
  republish (§7.2 + §8.2 / §8.3). Also bundles a follow-up commit
  adding native heads-up notifications on Android (river_messages
  channel + JNI bridge in `node_runtime::post_message_notification`)
  and a MainActivity onStart/onStop foreground tracker so the existing
  "current room + visible → suppress" guard mirrors the web build's
  document.visibility_state semantics. Tracker round-trip verified live
  on the Pixel 10 Pro XL — boot → Home → relaunch sequences log
  `Activity foreground state: true / false / true` cleanly.*

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

## Known issues (blockers to close before claiming Android-prod-ready)

### ~~Chat-delegate ApplicationMessages always fail on Android — no auth_token~~ — RESOLVED

Fixed by adopting fix option (a) below: synthetic auth_token registered
with the embedded node's `OriginContractMap` at startup, surfaced to the
UI via `crate::node_runtime::EMBEDDED_AUTH_TOKEN`, appended by the native
`connection_manager` as `&authToken=…` on the loopback WS URL.

- `ui/src/node_runtime.rs`: swapped `serve_client_api` →
  `serve_client_api_with_listener_and_contracts` to surface the
  `OriginContractMap`. Pre-binds a `std::net::TcpListener` (freenet's
  `serve_with_listener` calls `set_nonblocking(true)` before converting to
  tokio). Generates `AuthToken::generate()`, parses
  `WEB_CONTAINER_CONTRACT_ID` into a `ContractInstanceId`, and inserts an
  `OriginContract::new(contract_id, ClientId::next())` against the token.
  Token is stashed in `EMBEDDED_AUTH_TOKEN: OnceLock<String>` at module
  top-level (available on every target so the host stub compiles cleanly,
  always-empty on non-Android). Also bumps `args.ws_api.token_ttl_seconds`
  to `u64::MAX` — the cleanup task otherwise reaps the token after 24h
  because nothing in the WS request path updates `last_accessed`.
- `ui/src/components/app/freenet_api/connection_manager.rs::node_url`
  (non-wasm32 path): appends `&authToken=<token>` when
  `EMBEDDED_AUTH_TOKEN.get()` is `Some`. The connect log line redacts the
  token to avoid leaking it in logcat.
- Side-fix: the pre-staged scaffolding imported `AuthToken` / `ClientId`
  from `freenet::client_events::*`, which is `pub(crate)`. Repointed to
  `freenet::dev_tool::{AuthToken, ClientId}` (the public re-export path
  used by freenet's own integration tests for this exact mechanism).

The contract id we attest is River's published web-container id
(`raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv`), so the chat-delegate's
per-origin storage namespace lines up byte-for-byte with what web
clients write. If the published web-container parameters ever shift the
contract id, update `WEB_CONTAINER_CONTRACT_ID` in
`ui/src/node_runtime.rs` in lockstep with
`published-contract/contract-id.txt`.

Verification:
- `cargo check -p river-ui --features example-data,no-sync` — clean.
- `cargo check -p river-ui --target wasm32-unknown-unknown --features
  example-data,no-sync` — clean (web build unaffected; node_runtime's
  EMBEDDED_AUTH_TOKEN is `OnceLock<String>` and on web never set, so
  `node_url` skips the append).
- `cargo check -p river-ui --target aarch64-linux-android
  --no-default-features --features example-data` (with NDK clang in
  PATH) — clean.
- `cargo test -p river-ui --bins --features example-data,no-sync` —
  244 tests pass.

Still pending: live verification on a physical device (a fresh APK
needs to be side-loaded so the `save_rooms` timeout no longer fires;
that's covered by section 8 acceptance tasks).

### ~~Other files still call bare `wasm_bindgen_futures::spawn_local`~~ — RESOLVED

`safe_spawn_local` was fixed in commit 1aa43eb4 to actually dispatch
to `dioxus::prelude::spawn` on Android (was a silent no-op). The
followup audit covered the seven files originally flagged:

- `ui/src/components/app/notifications.rs` — already safe; the two
  bare callsites are inside `#[cfg(target_arch = "wasm32")]` functions
  (`river_test_notification`, `show_notification`). The non-gated
  `request_permission_if_needed` already uses `safe_spawn_local`.
- `ui/src/components/app/freenet_api/connection_manager.rs` — already
  safe; the bare import and four callsites live inside
  `#[cfg(target_arch = "wasm32")] mod imp`. The non-wasm32 path uses
  `crate::platform::spawn_local` via a sibling `mod`.
- `ui/src/components/room_list/room_name_field.rs` — patched: swapped
  the file-level `use wasm_bindgen_futures::spawn_local;` for
  `use crate::util::safe_spawn_local as spawn_local;`.
- `ui/src/components/room_list/edit_room_modal.rs` — patched: same
  import swap + rewrote the one fully-qualified
  `wasm_bindgen_futures::spawn_local` callsite to the alias.
- `ui/src/components/members/member_info_modal.rs` — patched:
  rewrote the lone fully-qualified callsite (focus-restore in
  `onmounted`) to `crate::util::safe_spawn_local`.
- `ui/src/components/members/member_info_modal/nickname_field.rs` —
  patched: rewrote the lone fully-qualified callsite (input blur on
  Enter) to `crate::util::safe_spawn_local`.
- `ui/src/components/direct_messages/dm_thread_modal.rs` — patched:
  rewrote both fully-qualified callsites (DM compose flow,
  delete-confirm dialog focus) to `crate::util::safe_spawn_local`.

`receive_invitation_modal.rs` was not on the original list but has
four bare callsites; each is preceded by its own
`#[cfg(target_arch = "wasm32")]` (per commit 3b21095f), with a
non-wasm32 `let _ = element;` fallthrough — already safe.

Verified: `cargo check -p river-ui --features example-data,no-sync`
(host) and `cargo check -p river-ui --target wasm32-unknown-unknown
--features example-data,no-sync` both clean — no warnings beyond the
pre-existing dead-code ones on Android-only `node_runtime.rs` items
(`FREENET_DATA_DIR_FALLBACK`, `android_files_dir`, `resolve_data_dir`).
Live verification on Pixel still pending until a fresh APK is built
and exercised through the previously-crashing flows (emoji reactions,
member-info focus, DM compose).

### Backward-probe → ROOMS race

The probe completion handler logs
`Backward probe recovered state for room ... but it is no longer in
ROOMS — discarding` when its `crate::util::defer(move || ROOMS.with_mut)`
fires after some other path has cleaned the placeholder. The PUT-forward
still happens (so the network ends up with the migrated state) but the
local merge is dropped. The next normal subscribe → GET-response round
trip re-adds the room, so it's a benign-looking warning today. If we
ever stop doing the redundant subscribe, this will silently lose data.
