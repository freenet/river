use super::constants::*;
use super::error::SynchronizerError;
use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
use crate::components::app::{AUTH_TOKEN, SYNC_STATUS, WEB_API};
use crate::util::sleep;
use dioxus::logger::tracing::{error, info};
use dioxus::prelude::*;
use freenet_stdlib::client_api::WebApi;
use futures::channel::mpsc::UnboundedSender;
use std::time::Duration;
use wasm_bindgen_futures::spawn_local;
use super::freenet_synchronizer;

/// Manages the connection to the Freenet node
pub struct ConnectionManager {
    connected: bool,
}

impl ConnectionManager {
    pub fn new() -> Self {
        info!("Creating new ConnectionManager");
        Self { connected: false }
    }

    /// Check if the connection is established and ready
    pub fn is_connected(&self) -> bool {
        *SYNC_STATUS.read() == SynchronizerStatus::Connected
    }

    /// Initializes the connection to the Freenet node
    pub async fn initialize_connection(
        &mut self,
        message_tx: UnboundedSender<freenet_synchronizer::SynchronizerMessage>,
    ) -> Result<(), SynchronizerError> {
        info!("Connecting to Freenet node at: {}", WEBSOCKET_URL);
        *SYNC_STATUS.write() = SynchronizerStatus::Connecting;
        self.connected = false;

        // Get the auth token and construct the URL with it if available
        let websocket_url = if let Some(token) = AUTH_TOKEN.read().clone() {
            info!("Adding authorization token to WebSocket URL");
            // Add the token as a query parameter
            if WEBSOCKET_URL.contains('?') {
                format!("{}&authorization=Bearer%20{}", WEBSOCKET_URL, token)
            } else {
                format!("{}?authorization=Bearer%20{}", WEBSOCKET_URL, token)
            }
        } else {
            info!("No authorization token available");
            WEBSOCKET_URL.to_string()
        };
        
        info!("Connecting to WebSocket URL: {}", websocket_url);
        let websocket = web_sys::WebSocket::new(&websocket_url).map_err(|e| {
            let error_msg = format!("Failed to create WebSocket: {:?}", e);
            error!("{}", error_msg);
            SynchronizerError::WebSocketError(error_msg)
        })?;

        // Create a simple oneshot channel for connection readiness
        let (ready_tx, ready_rx) = futures::channel::oneshot::channel();
        let message_tx_clone = message_tx.clone();

        // No need to create a reference to self.connected since we're not using it in callbacks

        info!("Starting WebAPI with callbacks");

        // Start the WebAPI
        let web_api = WebApi::start(
            websocket.clone(),
            move |result| {
                let mapped_result =
                    result.map_err(|e| SynchronizerError::WebSocketError(e.to_string()));
                let tx = message_tx_clone.clone();
                spawn_local(async move {
                    if let Err(e) = tx.unbounded_send(
                        freenet_synchronizer::SynchronizerMessage::ApiResponse(
                            mapped_result,
                        ),
                    ) {
                        error!("Failed to send API response: {}", e);
                    }
                });
            },
            {
                move |error| {
                    let error_msg = format!("WebSocket error: {}", error);
                    error!("{}", error_msg);
                    spawn_local(async move {
                        *SYNC_STATUS.write() =
                            freenet_synchronizer::SynchronizerStatus::Error(error_msg);
                    });
                }
            },
            {
                move || {
                    info!("WebSocket connected successfully");
                    spawn_local(async move {
                        *SYNC_STATUS.write() =
                            freenet_synchronizer::SynchronizerStatus::Connected;
                    });
                    let _ = ready_tx.send(());
                }
            },
        );

        // Wait for connection with timeout
        info!(
            "Waiting for connection with timeout of {}ms",
            CONNECTION_TIMEOUT_MS
        );

        // Create a timeout future
        let timeout_future = sleep(Duration::from_millis(CONNECTION_TIMEOUT_MS));

        // Race the ready signal against the timeout
        let result = futures::future::select(Box::pin(ready_rx), Box::pin(timeout_future)).await;

        match result {
            futures::future::Either::Left((Ok(_), _)) => {
                info!("WebSocket connection established successfully");
                *WEB_API.write() = Some(web_api);
                self.connected = true;
                *SYNC_STATUS.write() = freenet_synchronizer::SynchronizerStatus::Connected;
                Ok(())
            }
            _ => {
                let error = SynchronizerError::WebSocketError(
                    "WebSocket connection failed or timed out".to_string(),
                );
                error!("{}", error);
                self.connected = false;
                *SYNC_STATUS.write() =
                    freenet_synchronizer::SynchronizerStatus::Error(error.to_string());

                // Schedule reconnect
                let tx = message_tx.clone();
                spawn_local(async move {
                    info!(
                        "Scheduling reconnection attempt in {}ms",
                        RECONNECT_INTERVAL_MS
                    );
                    sleep(Duration::from_millis(RECONNECT_INTERVAL_MS)).await;
                    info!("Attempting reconnection now");
                    if let Err(e) =
                        tx.unbounded_send(freenet_synchronizer::SynchronizerMessage::Connect)
                    {
                        error!("Failed to send reconnect message: {}", e);
                    }
                });

                Err(error)
            }
        }
    }
}
