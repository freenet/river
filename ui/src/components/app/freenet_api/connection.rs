//! WebSocket connection management for Freenet API

use crate::components::app::freenet_api::types::{SYNC_STATUS, SyncStatus, WEBSOCKET_URL};
use freenet_stdlib::client_api::{HostResponse, WebApi};
use futures::channel::{mpsc, oneshot};
use futures::future;
use futures_timer::Delay;
use log::{error, info};
use std::time::Duration;

/// Initialize WebSocket connection to Freenet
pub async fn initialize_connection() -> Result<(web_sys::WebSocket, WebApi), String> {
    info!("Starting FreenetApiSynchronizer...");
    *SYNC_STATUS.write() = SyncStatus::Connecting;

    let websocket_connection = match web_sys::WebSocket::new(WEBSOCKET_URL) {
        Ok(ws) => {
            info!("WebSocket created successfully");
            ws
        },
        Err(e) => {
            let error_msg = format!("Failed to connect to WebSocket: {:?}", e);
            error!("{}", error_msg);
            *SYNC_STATUS.write() = SyncStatus::Error(error_msg.clone());
            return Err(error_msg);
        }
    };

    let (host_response_sender, _host_response_receiver) =
        mpsc::unbounded::<Result<HostResponse, String>>();

    // Create oneshot channels to know when the connection is ready
    let (_ready_tx, ready_rx) = oneshot::channel::<()>();
    let (ready_tx_clone, _) = oneshot::channel::<()>();

    let web_api = WebApi::start(
        websocket_connection.clone(),
        move |result| {
            let mut sender = host_response_sender.clone();
            wasm_bindgen_futures::spawn_local(async move {
                // Map ClientError to String
                let mapped_result = result.map_err(|e| e.to_string());
                if let Err(e) = sender.send(mapped_result).await {
                    error!("Failed to send host response: {}", e);
                }
            });
        },
        |error| {
            let error_msg = format!("WebSocket error: {}", error);
            error!("{}", error_msg);
            *SYNC_STATUS.write() = SyncStatus::Error(error_msg);
        },
        move || {
            info!("WebSocket connected successfully");
            *SYNC_STATUS.write() = SyncStatus::Connected;
            // Signal that the connection is ready
            let _ = ready_tx_clone.send(());
        },
    );

    // Wait for the connection to be ready or timeout
    match future::select(
        ready_rx,
        Delay::new(Duration::from_secs(5))
    ).await {
        future::Either::Left((_, _)) => {
            info!("WebSocket connection established successfully");
            Ok((websocket_connection, web_api))
        },
        future::Either::Right((_, _)) => {
            let error_msg = "WebSocket connection timed out".to_string();
            error!("{}", error_msg);
            *SYNC_STATUS.write() = SyncStatus::Error(error_msg.clone());
            Err(error_msg)
        }
    }
}
