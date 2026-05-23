//! Android-only embedded Freenet node.
//!
//! Spawns a dedicated tokio multi-thread runtime on a background OS thread
//! and runs `freenet::run_local_node` on it. The node binds its WebSocket
//! API at the default `127.0.0.1:7509`; River's `ConnectionManager` (native
//! impl in `freenet_api/connection_manager.rs`) connects to that endpoint.
//!
//! The node is *separate* from the Dioxus runtime — it owns its own tokio
//! reactor so the UI's event loop isn't sharing scheduling pressure with
//! wasmtime contract execution, and so that long-lived node tasks (the
//! event loop's `recv()` etc.) don't have to be `'static` against the
//! Dioxus scope.
//!
//! Phase-2 caveats (documented so the next phase picks them up cleanly):
//! - Storage path is hardcoded to the Android app's private files dir
//!   (`/data/data/org.freenet.river/files/freenet`). A proper port would
//!   query Android for this via JNI; the hardcoded path is correct for
//!   the published bundle identifier in `Dioxus.toml`.
//! - No bundled contract/delegate WASMs yet — until Phase 3 lands, the
//!   node will refuse to host River's room contract and chat delegate
//!   (they're not in its contract store). Sync will fail at the GET-
//!   contract stage. That's expected for this phase.
//! - No foreground service yet — Android may kill the process when the
//!   app backgrounds. Phase 6.

use std::path::PathBuf;
use std::sync::Arc;

use dioxus::logger::tracing::{error, info, warn};
use freenet::config::ConfigArgs;
use freenet::local_node::{Executor, OperationMode};

/// App-private storage dir for the bundled Freenet node.
///
/// Hardcoded to match the package id in `ui/Dioxus.toml` (`org.freenet.river`).
/// On a real device this resolves to `/data/data/org.freenet.river/files/freenet/`.
const FREENET_DATA_DIR: &str = "/data/data/org.freenet.river/files/freenet";

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

/// Build a local-mode `Config` and drive the node's event loop.
async fn run_node() -> anyhow::Result<()> {
    let data_dir = PathBuf::from(FREENET_DATA_DIR);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        warn!("Could not create node data dir {data_dir:?}: {e}");
        // Continue anyway — freenet's own setup will surface the error
        // through anyhow with full context.
    }

    let mut args = ConfigArgs {
        mode: Some(OperationMode::Local),
        ..ConfigArgs::default()
    };
    args.config_paths.config_dir = Some(data_dir.clone());
    args.config_paths.data_dir = Some(data_dir.clone());
    args.config_paths.log_dir = Some(data_dir.join("logs"));

    info!("Building freenet Config at {:?}", data_dir);
    let config = args.build().await?;
    let socket = config.ws_api.clone();
    info!("Constructing local executor (loads contract/delegate stores)");
    let executor = Executor::from_config_local(Arc::new(config))
        .await
        .map_err(anyhow::Error::msg)?;
    info!("Starting `run_local_node` on {:?}", socket.address);
    freenet::run_local_node(executor, socket)
        .await
        .map_err(anyhow::Error::msg)?;
    Ok(())
}
