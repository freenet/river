use super::constants::*;
use super::error::SynchronizerError;
use super::freenet_synchronizer;
use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
use crate::components::app::{AUTH_TOKEN, SYNC_STATUS, WEB_API};
use crate::util::sleep;
use dioxus::logger::tracing::{error, info};
use dioxus::prelude::*;
use freenet_stdlib::client_api::WebApi;
use futures::channel::mpsc::UnboundedSender;
use std::time::Duration;
use wasm_bindgen_futures::spawn_local;

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
        // Get auth token to add as query parameter
        let auth_token = AUTH_TOKEN.read().clone();
        let websocket_url = if let Some(token) = auth_token {
            // Check if the URL already has query parameters
            if WEBSOCKET_URL.contains('?') {
                format!("{}&authToken={}", WEBSOCKET_URL, token)
            } else {
                format!("{}?authToken={}", WEBSOCKET_URL, token)
            }
        } else {
            WEBSOCKET_URL.to_string()
        };

        info!("Connecting to Freenet node at: {}", websocket_url);
        *SYNC_STATUS.write() = SynchronizerStatus::Connecting;
        self.connected = false;

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

        info!("Starting WebAPI");
        
        // Convert web_sys WebSocket to tokio-tungstenite stream
        // This is a placeholder - we need to properly handle the WebSocket conversion
        // For now, we'll note that this needs to be fixed
        error!("WebSocket conversion from web_sys to tokio-tungstenite not implemented");
        return Err(SynchronizerError::WebSocketError("WebSocket type conversion not implemented".to_string()));
        
        // The following code should work once we have proper WebSocket conversion:
        /*
        let web_api = WebApi::start(ws_stream);
        
        // Spawn a task to handle incoming messages
        let message_tx_for_recv = message_tx_clone.clone();
        spawn_local(async move {
            loop {
                match web_api.recv().await {
                    Ok(response) => {
                        let mapped_result = Ok(response);
                        if let Err(e) = message_tx_for_recv.unbounded_send(
                            freenet_synchronizer::SynchronizerMessage::ApiResponse(mapped_result),
                        ) {
                            error!("Failed to send API response: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        let mapped_result = Err(SynchronizerError::WebSocketError(e.to_string()));
                        if let Err(e) = message_tx_for_recv.unbounded_send(
                            freenet_synchronizer::SynchronizerMessage::ApiResponse(mapped_result),
                        ) {
                            error!("Failed to send API error: {}", e);
                        }
                        break;
                    }
                }
            }
        });
        
        // Mark as connected
        info!("WebSocket connected successfully");
        spawn_local(async move {
            *SYNC_STATUS.write() = freenet_synchronizer::SynchronizerStatus::Connected;
        });
        let _ = ready_tx.send(());
        */

        /* Commented out until WebSocket conversion is implemented
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
                *SYNC_STATUS.write() = SynchronizerStatus::Connected;

                // Now that we're connected, send the auth token
                /* Disabled because it's generating an error from the API, use URL query param instead above
                let auth_token = AUTH_TOKEN.read().clone();
                if let Some(token) = auth_token {
                    info!("Sending auth token to WebSocket");
                    if let Some(api) = &mut *WEB_API.write() {
                        match api.send(ClientRequest::Authenticate { token }).await {
                            Ok(_) => info!("Authentication token sent successfully"),
                            Err(e) => {
                                // Check if this is a "not supported" error
                                if e.to_string().contains("not supported") {
                                    warn!("Authentication method not supported by server. This may indicate API version mismatch.");
                                    // Continue anyway as some operations might still work
                                    info!("Continuing despite authentication error");
                                } else {
                                    return Err(e.into());
                                }
                            }
                        }
                    }
                } */

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
        */
    }
}
