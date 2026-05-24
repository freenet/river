//! Embedded Freenet node (Android-only at runtime, portable at the
//! type level so host `cargo test` can exercise the no-JNI fallback
//! paths).
//!
//! On Android, `start_embedded_node` spawns a dedicated tokio
//! multi-thread runtime on a background OS thread and drives the
//! freenet node's network-mode event loop on it. The node binds its
//! WebSocket client API at the default `127.0.0.1:7509`; River's
//! `ConnectionManager` (native impl in
//! `freenet_api/connection_manager.rs`) connects to that endpoint.
//!
//! The node is *separate* from the Dioxus runtime — it owns its own
//! tokio reactor so the UI's event loop isn't sharing scheduling
//! pressure with wasmtime contract execution, and so that long-lived
//! node tasks (peer connection recv loops, transport drivers) don't
//! have to be `'static` against the Dioxus scope.
//!
//! **Network mode, not Local.** A Local-mode node only serves what's
//! been PUT to its own stores — Android users could create their own
//! rooms but could not join any room shared via invitation link,
//! because the network state never reaches their device. Network
//! mode makes the device a real Freenet peer that fetches contracts
//! and states through peers and gateways. See
//! `openspec/changes/android-bundled-node/design.md` decision #2 for
//! the full rationale and the mobile-NAT risks.
//!
//! Remaining caveats (tracked in the OpenSpec change's tasks.md):
//! - No foreground service yet — Android may kill the process when
//!   the app backgrounds (tasks 5.x).
//!
//! On non-Android targets, `start_embedded_node` is a no-op stub so
//! the module compiles for host `cargo check` / `cargo test` without
//! pulling in freenet, tokio, jni, or ndk-context.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Synthetic auth token registered with the embedded node's
/// `OriginContractMap` at startup, surfaced here so the UI's loopback
/// WebSocket dial can attach it as `?authToken=…`.
///
/// **Why this exists.** The chat-delegate's `check_origin` rejects any
/// `DelegateRequest::ApplicationMessage` whose `MessageOrigin` is
/// `None` with `"missing message origin"`. On the web build the gateway
/// shell injects `window.__FREENET_AUTH_TOKEN__`, and the WS handler
/// looks the token up in a map populated when the shell HTML was served
/// to mint the page's contract origin. On Android the UI loads via wry's
/// custom protocol, never goes through a gateway shell, and would
/// otherwise dial the loopback WS anonymously — every delegate save
/// times out and the room is lost on restart.
///
/// We close that gap by pre-registering a random token under River's
/// **published web-container contract id** (so the attested origin
/// matches what web clients send) in `serve_client_api_with_listener_and_contracts`'s
/// returned map, then publishing the token here for
/// `connection_manager` to pick up. `OnceLock` because the node starts
/// exactly once per process and the token is immutable thereafter.
pub static EMBEDDED_AUTH_TOKEN: OnceLock<String> = OnceLock::new();

/// Hardcoded fallback for the embedded Freenet node's storage dir.
///
/// Matches the package id in `ui/Dioxus.toml` (`org.freenet.river`).
/// On a real device the runtime path comes from
/// `Context.getFilesDir()` via JNI ([`android_files_dir`]) — this
/// constant is only used when the JNI lookup fails (emulator without
/// an attached Activity, unusual launch path, etc.) so the node can
/// still boot rather than aborting.
pub(crate) const FREENET_DATA_DIR_FALLBACK: &str =
    "/data/data/org.freenet.river/files/freenet";

/// Resolve the app's private files dir at runtime via JNI.
///
/// Walks the standard Android startup handles:
/// 1. `ndk_context::android_context()` exposes the `JavaVM` + the
///    activity's `Context` jobject, published by ndk-glue / wry / tao
///    during native startup.
/// 2. Attach the current thread to the VM.
/// 3. Call `Context.getFilesDir() -> java/io/File`.
/// 4. Call `File.getAbsolutePath() -> java/lang/String`.
/// 5. Decode the Java string to a Rust `String` and join `"freenet"`
///    onto it, giving the node its own subdirectory under the app's
///    private files area.
///
/// Returns `None` on any JNI failure (ndk-context not populated, VM
/// attach failure, `NoSuchMethodError`) AND on every non-Android
/// target. Callers fall back to [`FREENET_DATA_DIR_FALLBACK`] and
/// emit a `warn!` so the failure is visible in logcat.
#[cfg(target_os = "android")]
pub(crate) fn android_files_dir() -> Option<PathBuf> {
    use jni::objects::{JObject, JString};
    use jni::JavaVM;

    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    let mut env = vm.attach_current_thread().ok()?;

    // Context.getFilesDir() -> java/io/File
    let files_dir = env
        .call_method(&activity, "getFilesDir", "()Ljava/io/File;", &[])
        .ok()?
        .l()
        .ok()?;

    // File.getAbsolutePath() -> java/lang/String
    let path_jstr = env
        .call_method(&files_dir, "getAbsolutePath", "()Ljava/lang/String;", &[])
        .ok()?
        .l()
        .ok()?;

    let path_jstr: JString = path_jstr.into();
    let rust_path: String = env.get_string(&path_jstr).ok()?.into();

    Some(PathBuf::from(rust_path).join("freenet"))
}

/// Host stub: no Activity, no JNI, always return `None` so the
/// caller falls back to [`FREENET_DATA_DIR_FALLBACK`].
#[cfg(not(target_os = "android"))]
pub(crate) fn android_files_dir() -> Option<PathBuf> {
    None
}

/// Compute the node's storage dir, with the JNI lookup tried first
/// and a fall-back to the hardcoded package-private path on failure.
pub(crate) fn resolve_data_dir() -> PathBuf {
    android_files_dir().unwrap_or_else(|| PathBuf::from(FREENET_DATA_DIR_FALLBACK))
}

#[cfg(target_os = "android")]
mod android {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    use dioxus::logger::tracing::{error, info, warn};
    // `freenet::client_events` is `pub(crate)`; the public re-exports
    // for `AuthToken` and `ClientId` live in `freenet::dev_tool` (alongside
    // the other types meant for external integration tests). The
    // OriginContract<AuthToken, ContractInstanceId> tuple we construct
    // below is exactly what the existing integration tests use to
    // pre-populate the map, so these are the right entry points.
    use freenet::config::ConfigArgs;
    use freenet::dev_tool::{AuthToken, ClientId};
    use freenet::local_node::{NodeConfig, OperationMode};
    use freenet::server::{
        OriginContract, serve_client_api_with_listener_and_contracts,
    };
    use freenet_stdlib::prelude::ContractInstanceId;
    use std::str::FromStr;
    use tokio::sync::oneshot;

    /// Base58 contract id of River's published web-container contract
    /// (`raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv`, the same id
    /// `published-contract/contract-id.txt` records). We attest this id
    /// to the chat delegate as the embedded UI's origin so the
    /// delegate's per-origin storage namespace (`signing_key:{origin}:…`,
    /// `outbound_dms:{origin}:…`, etc.) matches what web clients use —
    /// keeps the namespace coherent if delegate state ever syncs across
    /// devices, and keeps the chat-delegate's `check_origin` guard
    /// satisfied. If the published web-container parameters ever shift
    /// the contract id, update both files in lockstep.
    const WEB_CONTAINER_CONTRACT_ID: &str =
        "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";

    /// Park for the lifetime of the process so that
    /// `Java_dev_dioxus_main_RiverNodeService_nativeOnServiceStop` can
    /// fire the oneshot from a foreign thread (the JNI callback runs on
    /// whatever thread the Android service is destroyed on, NOT the
    /// freenet-worker tokio runtime).
    ///
    /// Populated once `run_node()` reaches its `select!` point and parks
    /// the receiver. If the user hits Home and then the foreground
    /// service is destroyed before the node has finished booting,
    /// `nativeOnServiceStop` finds `None` in the Mutex and short-circuits —
    /// the process will be killed by the OS anyway and there's nothing
    /// for us to gracefully drop.
    static SHUTDOWN_TX: Mutex<Option<oneshot::Sender<()>>> = Mutex::new(None);

    /// Fallback `gateways.toml` and its referenced X25519 public-key files,
    /// snapshotted from `https://freenet.org/keys/` at release time.
    ///
    /// Used only on first launch IF freenet's auto-fetch from
    /// `freenet.org` fails (offline, DNS blocked, etc.). Once the live
    /// fetch succeeds, freenet overwrites `config_dir/gateways.toml` with
    /// the freshly-fetched paths and these fallbacks are unused. See
    /// `vendor/freenet/src/config.rs::load_gateways_from_index` for the
    /// canonical fetch behaviour and the local-cache fallback path that
    /// reads this file on retry.
    ///
    /// Refresh procedure: re-fetch from
    /// `https://freenet.org/keys/{gateways.toml,public.nova.gw.pem,public.vega.gw.pem}`
    /// when cutting a release, replace the three files under
    /// `ui/assets/freenet/`, and verify a debug APK can bootstrap with
    /// `freenet.org` DNS blocked.
    const FALLBACK_GATEWAYS_TOML: &[u8] = include_bytes!("../assets/freenet/gateways.toml");
    const FALLBACK_NOVA_PUBKEY: &[u8] = include_bytes!("../assets/freenet/public.nova.gw.pem");
    const FALLBACK_VEGA_PUBKEY: &[u8] = include_bytes!("../assets/freenet/public.vega.gw.pem");

    /// Boot the in-process Freenet node on a dedicated background thread.
    ///
    /// Returns immediately; the node runs for the lifetime of the process
    /// (or until an unrecoverable error, which is logged). Safe to call
    /// multiple times — guarded by a one-shot.
    pub fn start_embedded_node() {
        use std::sync::Once;
        static START: Once = Once::new();
        START.call_once(|| {
            info!("Spawning embedded Freenet node thread");
            let handle = std::thread::Builder::new()
                .name("freenet-embedded".into())
                .stack_size(4 * 1024 * 1024) // wasmtime needs >stack default
                .spawn(|| {
                    let rt = match tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .thread_name("freenet-worker")
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            error!("Failed to build tokio runtime for embedded node: {e}");
                            return;
                        }
                    };
                    rt.block_on(async move {
                        match run_node().await {
                            Ok(()) => info!("Embedded Freenet node exited cleanly"),
                            Err(e) => {
                                error!("Embedded Freenet node exited with error: {e:?}")
                            }
                        }
                    });
                });
            if let Err(e) = handle {
                error!("Failed to spawn freenet-embedded thread: {e}");
            }
        });
    }

    /// Build a network-mode `Config` and drive the node's event loop.
    ///
    /// Mirrors `freenet/src/bin/freenet.rs::run_network` with one
    /// Android-specific twist (step 1.5):
    ///   1. Pre-bind the WS API listener AND grab the
    ///      `OriginContractMap` via
    ///      `serve_client_api_with_listener_and_contracts`. We use this
    ///      entry point (not the simpler `serve_client_api`) because the
    ///      map is the only way to attest an origin to the chat delegate
    ///      without a gateway shell.
    ///   1.5. Insert a synthetic auth_token entry into the map under
    ///      River's web-container contract id, and publish the token via
    ///      [`EMBEDDED_AUTH_TOKEN`] so the UI's loopback dial can append
    ///      `?authToken=…`. See [`EMBEDDED_AUTH_TOKEN`]'s doc for why.
    ///   2. `NodeConfig::new` loads peer-state config (gateway list,
    ///      peer id, etc.).
    ///   3. `node_config.build(clients)` wires the client API into the
    ///      node.
    ///   4. `freenet::run_network_node` drives the event loop forever.
    async fn run_node() -> anyhow::Result<()> {
        let data_dir = resolve_data_dir();
        if android_files_dir().is_none() {
            warn!(
                "JNI lookup for Context.getFilesDir() failed; falling back to {}. \
                 The node will still try to boot, but if the package id ever drifts \
                 from `org.freenet.river` (see ui/Dioxus.toml) writes will land in \
                 the wrong place. This usually means an emulator launched without \
                 an attached Activity, or the ndk-context handles weren't populated.",
                FREENET_DATA_DIR_FALLBACK
            );
        }
        if let Err(e) = std::fs::create_dir_all(&data_dir) {
            warn!("Could not create node data dir {data_dir:?}: {e}");
            // Continue anyway — freenet's own setup will surface the
            // error through anyhow with full context.
        }

        // Stage the fallback `gateways.toml` + PEMs into the node's
        // config dir BEFORE `args.build()`, because `ConfigArgs::build`
        // is itself what loads the gateway list — if neither a live
        // fetch nor a local cache produces one, build() returns
        // `Cannot initialize node without gateways` and we never even
        // get a `Config` to inspect. The config dir layout is fixed
        // (see vendor/freenet/src/config.rs::ConfigPaths::build):
        //
        //   config_dir  = data_dir            (the path we set above)
        //   secrets_dir = data_dir.join("secrets")
        //
        // Best-effort: failures are logged but don't abort startup,
        // because freenet's own first-launch HTTPS fetch from
        // `freenet.org` is the primary path. The bundled fallback
        // only matters when first launch is offline (no network).
        let config_dir = data_dir.clone();
        let secrets_dir = data_dir.join("secrets");
        if let Err(e) = stage_fallback_gateways(&config_dir, &secrets_dir) {
            warn!("Could not stage fallback gateways: {e}. Live fetch will be attempted.");
        }

        let mut args = ConfigArgs {
            mode: Some(OperationMode::Network),
            ..ConfigArgs::default()
        };
        args.config_paths.config_dir = Some(data_dir.clone());
        args.config_paths.data_dir = Some(data_dir.clone());
        args.config_paths.log_dir = Some(data_dir.join("logs"));
        // Effectively disable the token-expiry sweep. The synthetic
        // auth_token we register below is loopback-only, never leaves the
        // device, and nothing in the WS request path updates the entry's
        // `last_accessed` field — at the default 24h TTL the cleanup task
        // would silently reap it and every subsequent ApplicationMessage
        // would start failing with `missing message origin` again. Set
        // `u64::MAX` so the cleanup retain-comparison never evicts. (No
        // overflow: `Duration::from_secs(u64::MAX)` saturates, and
        // `elapsed < ttl` is always true.)
        args.ws_api.token_ttl_seconds = Some(u64::MAX);

        info!("Building freenet network Config at {:?}", data_dir);
        let config = args.build().await?;
        let ws_socket = config.ws_api.clone();

        // Pre-bind the WS API listener ourselves so we can hand it to
        // `serve_client_api_with_listener_and_contracts`. That entry
        // point returns the `OriginContractMap` we need to populate
        // with the synthetic auth_token before any request lands; the
        // shorter `serve_client_api(config)` would let freenet bind
        // internally but doesn't surface the map. Freenet's
        // `serve_with_listener` calls `set_nonblocking(true)` on the
        // listener before `tokio::net::TcpListener::from_std`, so we
        // pass a plain blocking listener here.
        info!("Starting client API on {:?}:{}", ws_socket.address, ws_socket.port);
        let listener = std::net::TcpListener::bind((ws_socket.address, ws_socket.port))
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to bind WS API listener on {}:{} ({e}). \
                     If another freenet process is already running on this device, \
                     stop it before relaunching River.",
                    ws_socket.address,
                    ws_socket.port,
                )
            })?;
        let (clients, origin_contracts) =
            serve_client_api_with_listener_and_contracts(ws_socket, listener)
                .await
                .map_err(|e| anyhow::anyhow!("failed to start client API: {e}"))?;

        // Pre-register a synthetic auth_token so the chat delegate's
        // `check_origin` finds an attested `MessageOrigin::WebApp(contract_id)`
        // on every ApplicationMessage, instead of `None` (which the delegate
        // rejects with "missing message origin" → every save times out → the
        // user's room dies on relaunch).
        //
        // The contract id we attest is River's published web-container id,
        // so the chat-delegate's per-origin storage namespace
        // (`signing_key:{origin}:…`, `outbound_dms:{origin}:…`, etc.) lines
        // up byte-for-byte with what web clients write — keeps delegate state
        // coherent if it ever syncs across devices, and is the same gate web
        // clients hit (since the gateway shell attests this same id).
        //
        // Fatal-on-failure intentional: if the contract-id constant ever drifts
        // out of `bs58` decode shape we want the node boot to fail loudly,
        // because every delegate request after this would be silently broken.
        let contract_id = ContractInstanceId::from_str(WEB_CONTAINER_CONTRACT_ID)
            .map_err(|e| {
                anyhow::anyhow!(
                    "WEB_CONTAINER_CONTRACT_ID ({WEB_CONTAINER_CONTRACT_ID:?}) failed to \
                     parse as base58: {e}. The constant must stay in sync with \
                     `published-contract/contract-id.txt`."
                )
            })?;
        let auth_token = AuthToken::generate();
        origin_contracts.insert(
            auth_token.clone(),
            OriginContract::new(contract_id, ClientId::next()),
        );
        let token_string = auth_token.as_str().to_string();
        // `OnceLock::set` is idempotent at the call-site we control
        // (`Once`-guarded `start_embedded_node`); if a future change ever
        // double-boots the node we silently keep the first token, since
        // the URL-builder in `connection_manager` already cached it.
        let _ = EMBEDDED_AUTH_TOKEN.set(token_string);
        info!(
            "Synthetic auth_token registered against {} \
             ({} entries in origin_contracts)",
            WEB_CONTAINER_CONTRACT_ID,
            origin_contracts.len(),
        );

        info!("Initialising NodeConfig (loads gateways.toml, derives peer id)");
        let node_config = NodeConfig::new(config).await?;

        info!("Building network node");
        let node = node_config.build(clients).await?;

        // Park the shutdown receiver before entering the event loop so
        // that a service-stop intent landing the instant after the
        // notification appears can still tear us down. The lock is held
        // only across the assignment.
        let (tx, shutdown_rx) = oneshot::channel::<()>();
        *SHUTDOWN_TX.lock().expect("SHUTDOWN_TX poisoned") = Some(tx);

        info!("Running network node event loop (with foreground-service shutdown hook)");
        tokio::select! {
            res = freenet::run_network_node(node) => {
                res?;
            }
            _ = shutdown_rx => {
                info!("Embedded node shutdown requested by RiverNodeService.onDestroy");
            }
        }
        Ok(())
    }

    /// JNI hook invoked from `RiverNodeService.onDestroy()` (see
    /// `ui/android/kotlin/dev/dioxus/main/RiverNodeService.kt`).
    ///
    /// Fires the parked oneshot so the freenet-worker tokio runtime
    /// drops the network node + transport drivers in an orderly fashion
    /// rather than being SIGKILL'd by Android. Best-effort: if the node
    /// hasn't reached `run_network_node` yet (e.g. user mashed Stop
    /// during boot), the sender is `None` and we no-op.
    ///
    /// JNI ABI: the function name must match the fully-qualified
    /// Java/Kotlin class + method name, with `.` replaced by `_`. Any
    /// rename on either side must be made in lock-step.
    ///
    /// Signature uses the raw `jni::sys` C types so we don't carry a
    /// `JNIEnv<'local>` lifetime through a `#[no_mangle] extern "system"`
    /// — we never call back into the JVM from this function, so the
    /// raw pointers are all we need.
    #[no_mangle]
    pub unsafe extern "system" fn Java_dev_dioxus_main_RiverNodeService_nativeOnServiceStop(
        _env: *mut jni::sys::JNIEnv,
        _class: jni::sys::jclass,
    ) {
        match SHUTDOWN_TX.lock() {
            Ok(mut slot) => match slot.take() {
                Some(tx) => {
                    if tx.send(()).is_err() {
                        warn!("Embedded node already gone — shutdown signal dropped");
                    } else {
                        info!("Shutdown signal sent to embedded node");
                    }
                }
                None => {
                    info!(
                        "RiverNodeService.onDestroy fired before embedded node reached \
                         the event loop — nothing to signal"
                    );
                }
            },
            Err(e) => {
                error!("SHUTDOWN_TX mutex poisoned: {e}");
            }
        }
    }

    /// Stage the bundled fallback `gateways.toml` + PEMs into
    /// `config_dir` and `secrets_dir`, ONLY if
    /// `config_dir/gateways.toml` doesn't already exist.
    ///
    /// Freenet's [`NodeConfig::new`] tries the live remote fetch
    /// first; on success, it overwrites `config_dir/gateways.toml`
    /// (and the PEMs in `secrets_dir`) with the freshly-fetched
    /// copy. On failure, it falls back to parsing whatever is
    /// already at `config_dir/gateways.toml`. By pre-staging the
    /// bundled fallback when that file is absent, we guarantee an
    /// offline first-launch still has a valid gateways list to
    /// parse — without it the node would error out with `Cannot
    /// initialize node without gateways`.
    ///
    /// We do NOT overwrite an existing `gateways.toml`: any file
    /// already at that path is freenet's own cache from a prior
    /// successful fetch and is at least as fresh as our bundle.
    ///
    /// The bundled PEMs match the snapshot in `ui/assets/freenet/`.
    /// If the live fetch later succeeds, freenet overwrites the
    /// same PEM filenames in `secrets_dir` with the fresh content —
    /// our stale bytes don't linger.
    fn stage_fallback_gateways(config_dir: &Path, secrets_dir: &Path) -> std::io::Result<()> {
        let gateways_file = config_dir.join("gateways.toml");
        if gateways_file.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(config_dir)?;
        std::fs::create_dir_all(secrets_dir)?;

        let nova_path = secrets_dir.join("public.nova.gw.pem");
        let vega_path = secrets_dir.join("public.vega.gw.pem");
        std::fs::write(&nova_path, FALLBACK_NOVA_PUBKEY)?;
        std::fs::write(&vega_path, FALLBACK_VEGA_PUBKEY)?;

        // Build the TOML with absolute paths. Freenet's local-cache
        // parser deserializes `public_key` straight into a `PathBuf`
        // and opens the file with no further path resolution;
        // relative paths would be resolved against the CWD, which
        // is undefined on Android.
        let toml = format!(
            "# Bundled fallback (used because freenet's live fetch failed).\n\
             [[gateways]]\n\
             public_key = \"{}\"\n\
             [gateways.address]\n\
             hostname = \"nova.locut.us:31337\"\n\
             \n\
             [[gateways]]\n\
             public_key = \"{}\"\n\
             [gateways.address]\n\
             hostname = \"vega.locut.us:31337\"\n",
            nova_path.display(),
            vega_path.display(),
        );
        std::fs::write(&gateways_file, toml)?;

        info!(
            "Staged fallback gateways.toml + 2 PEMs at {:?} ({} bytes bundled)",
            gateways_file,
            FALLBACK_GATEWAYS_TOML.len(),
        );
        Ok(())
    }
}

#[cfg(target_os = "android")]
pub use android::start_embedded_node;

/// Non-Android stub. The Android startup path in `App()` is itself
/// `cfg(target_os = "android")`-gated, so this stub is unreachable
/// in practice — but exposing it lets the module compile for host
/// `cargo check` / `cargo test` without dragging in freenet, tokio,
/// jni, or ndk-context.
#[cfg(not(target_os = "android"))]
#[allow(dead_code)]
pub fn start_embedded_node() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_path_targets_known_package_id() {
        // The hardcoded fallback must point at the package id River
        // declares in `ui/Dioxus.toml` — if that identifier ever
        // changes, this constant has to follow or the bundled node
        // will write to a directory Android won't grant access to.
        assert!(
            FREENET_DATA_DIR_FALLBACK.contains("/org.freenet.river/"),
            "fallback {FREENET_DATA_DIR_FALLBACK} no longer targets org.freenet.river"
        );
        assert!(
            FREENET_DATA_DIR_FALLBACK.ends_with("/freenet"),
            "fallback {FREENET_DATA_DIR_FALLBACK} must end with /freenet \
             so the node has its own subdir under the app's private files area"
        );
    }

    /// On non-Android targets `android_files_dir` is the no-op stub,
    /// so `resolve_data_dir` MUST return the hardcoded fallback.
    /// Gated to non-android because on a real device the JNI lookup
    /// succeeds and returns a different (real) path — the assertion
    /// would not hold there.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn resolve_data_dir_returns_fallback_off_device() {
        assert!(android_files_dir().is_none(), "host stub must return None");
        let dir = resolve_data_dir();
        assert_eq!(dir, PathBuf::from(FREENET_DATA_DIR_FALLBACK));
    }
}
