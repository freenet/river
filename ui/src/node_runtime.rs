//! Android-only embedded Freenet node.
//!
//! Spawns a dedicated tokio multi-thread runtime on a background OS thread
//! and drives the freenet node's network-mode event loop on it. The node
//! binds its WebSocket client API at the default `127.0.0.1:7509`; River's
//! `ConnectionManager` (native impl in `freenet_api/connection_manager.rs`)
//! connects to that endpoint.
//!
//! The node is *separate* from the Dioxus runtime — it owns its own tokio
//! reactor so the UI's event loop isn't sharing scheduling pressure with
//! wasmtime contract execution, and so that long-lived node tasks (peer
//! connection recv loops, transport drivers) don't have to be `'static`
//! against the Dioxus scope.
//!
//! **Network mode, not Local.** A Local-mode node only serves what's been
//! PUT to its own stores — Android users could create their own rooms but
//! could not join any room shared via invitation link, because the network
//! state never reaches their device. Network mode makes the device a real
//! Freenet peer that fetches contracts and states through peers and
//! gateways. See `openspec/changes/android-bundled-node/design.md`
//! decision #2 for the full rationale and the mobile-NAT risks.
//!
//! Remaining caveats (tracked in the OpenSpec change's tasks.md):
//! - Storage path is hardcoded to the Android app's private files dir
//!   (`/data/data/org.freenet.river/files/freenet`). A proper port queries
//!   Android for this via JNI (tasks 4.x).
//! - On first launch the freenet config fetches `gateways.toml` from
//!   `https://freenet.org/keys/gateways.toml`. A fallback APK-bundled
//!   snapshot for offline-first-launch is task 3.2/3.3.
//! - No foreground service yet — Android may kill the process when the
//!   app backgrounds (tasks 5.x).

use std::path::{Path, PathBuf};

use dioxus::logger::tracing::{error, info, warn};
use freenet::config::ConfigArgs;
use freenet::local_node::{NodeConfig, OperationMode};
use freenet::server::serve_client_api;

/// App-private storage dir for the bundled Freenet node.
///
/// Hardcoded to match the package id in `ui/Dioxus.toml` (`org.freenet.river`).
/// On a real device this resolves to `/data/data/org.freenet.river/files/freenet/`.
const FREENET_DATA_DIR: &str = "/data/data/org.freenet.river/files/freenet";

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
                        Err(e) => error!("Embedded Freenet node exited with error: {e:?}"),
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
/// Mirrors `freenet/src/bin/freenet.rs::run_network`:
///   1. `serve_client_api` binds the loopback WebSocket the UI dials.
///   2. `NodeConfig::new` loads peer-state config (gateway list, peer
///      id, etc.).
///   3. `node_config.build(clients)` wires the client API into the node.
///   4. `freenet::run_network_node` drives the event loop forever.
async fn run_node() -> anyhow::Result<()> {
    let data_dir = PathBuf::from(FREENET_DATA_DIR);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        warn!("Could not create node data dir {data_dir:?}: {e}");
        // Continue anyway — freenet's own setup will surface the error
        // through anyhow with full context.
    }

    let mut args = ConfigArgs {
        mode: Some(OperationMode::Network),
        ..ConfigArgs::default()
    };
    args.config_paths.config_dir = Some(data_dir.clone());
    args.config_paths.data_dir = Some(data_dir.clone());
    args.config_paths.log_dir = Some(data_dir.join("logs"));

    info!("Building freenet network Config at {:?}", data_dir);
    let config = args.build().await?;
    let ws_socket = config.ws_api.clone();

    // Stage the fallback `gateways.toml` + PEMs into the node's config
    // dir IF nothing is there yet. Best-effort: failures are logged but
    // don't abort startup, because freenet's own first-launch HTTPS
    // fetch from `freenet.org` is the primary path and usually
    // succeeds. The bundled fallback only kicks in when first launch
    // is offline (no network), where it lets the node boot anyway.
    if let Err(e) = stage_fallback_gateways(&config.config_dir(), &config.secrets_dir()) {
        warn!("Could not stage fallback gateways: {e}. Live fetch will be attempted.");
    }

    info!("Starting client API on {:?}", ws_socket.address);
    let clients = serve_client_api(ws_socket)
        .await
        .map_err(|e| anyhow::anyhow!("failed to start client API: {e}"))?;

    info!("Initialising NodeConfig (loads gateways.toml, derives peer id)");
    let node_config = NodeConfig::new(config).await?;

    info!("Building network node");
    let node = node_config.build(clients).await?;

    info!("Running network node event loop");
    freenet::run_network_node(node).await?;
    Ok(())
}

/// Stage the bundled fallback `gateways.toml` + PEMs into `config_dir`
/// and `secrets_dir`, ONLY if `config_dir/gateways.toml` doesn't
/// already exist.
///
/// Freenet's [`NodeConfig::new`] tries the live remote fetch first;
/// on success, it overwrites `config_dir/gateways.toml` (and the PEMs
/// in `secrets_dir`) with the freshly-fetched copy. On failure, it
/// falls back to parsing whatever is already at
/// `config_dir/gateways.toml`. By pre-staging the bundled fallback
/// when that file is absent, we guarantee an offline first-launch
/// still has a valid gateways list to parse — without it the node
/// would error out with `Cannot initialize node without gateways`.
///
/// We do NOT overwrite an existing `gateways.toml`: any file already
/// at that path is freenet's own cache from a prior successful fetch
/// and is at least as fresh as our bundle.
///
/// The bundled PEMs match the snapshot in `ui/assets/freenet/`. If
/// the live fetch later succeeds, freenet overwrites the same PEM
/// filenames in `secrets_dir` with the fresh content — our stale
/// bytes don't linger.
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

    // Build the TOML with absolute paths. Freenet's local-cache parser
    // deserializes `public_key` straight into a `PathBuf` and opens
    // the file with no further path resolution; relative paths would
    // be resolved against the CWD, which is undefined on Android.
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
