use super::constants::*;
use super::error::SynchronizerError;
use dioxus::prelude::*;
use dioxus::logger::tracing::{info, error};
use std::time::Duration;
use wasm_bindgen_futures::spawn_local;
use futures::channel::mpsc::UnboundedSender;
use futures::FutureExt;
use freenet_stdlib::client_api::WebApi;
use crate::util::sleep;

/// Manages the connection to the Freenet node
pub struct ConnectionManager {
    web_api: Option<WebApi>,
    synchronizer_status: Signal<super::freenet_synchronizer::SynchronizerStatus>,
    connected: bool,
}

impl ConnectionManager {
    pub fn new(
        synchronizer_status: Signal<super::freenet_synchronizer::SynchronizerStatus>,
    ) -> Self {
        info!("Creating new ConnectionManager");
        Self {
            web_api: None,
            synchronizer_status,
            connected: false,
        }
    }
    
    /// Check if the connection is established and ready
    pub fn is_connected(&self) -> bool {
        self.connected && self.web_api.is_some()
    }

    /// Initializes the connection to the Freenet node
    pub async fn initialize_connection(
        &mut self,
        message_tx: UnboundedSender<super::freenet_synchronizer::SynchronizerMessage>,
    ) -> Result<(), SynchronizerError> {
        info!("Connecting to Freenet node at: {}", WEBSOCKET_URL);
        *self.synchronizer_status.write() = super::freenet_synchronizer::SynchronizerStatus::Connecting;
        self.connected = false;
        
        let websocket = web_sys::WebSocket::new(WEBSOCKET_URL).map_err(|e| {
            let error_msg = format!("Failed to create WebSocket: {:?}", e);
            error!("{}", error_msg);
            SynchronizerError::WebSocketError(error_msg)
        })?;

        // Create a simple oneshot channel for connection readiness
        let (ready_tx, ready_rx) = futures::channel::oneshot::channel();
        let message_tx_clone = message_tx.clone();
        
        // Create a copy of the status signal for use in callbacks
        let sync_status_clone = self.synchronizer_status.clone();
        
        // Create a reference to self.connected for the open handler
        let connected_ref = &mut self.connected;
        
        info!("Starting WebAPI with callbacks");
        
        // Start the WebAPI
        let web_api = WebApi::start(
            websocket.clone(),
            move |result| {
                let mapped_result = result.map_err(|e| SynchronizerError::WebSocketError(e.to_string()));
                let tx = message_tx_clone.clone();
                spawn_local(async move {
                    if let Err(e) = tx.unbounded_send(super::freenet_synchronizer::SynchronizerMessage::ApiResponse(mapped_result)) {
                        error!("Failed to send API response: {}", e);
                    }
                });
            },
            {
                let status = sync_status_clone.clone();
                move |error| {
                    let error_msg = format!("WebSocket error: {}", error);
                    error!("{}", error_msg);
                    let mut status_copy = status.clone();
                    spawn_local(async move {
                        *status_copy.write() = super::freenet_synchronizer::SynchronizerStatus::Error(error_msg);
                    });
                }
            },
            {
                let status = sync_status_clone.clone();
                move || {
                    info!("WebSocket connected successfully");
                    let mut status_copy = status.clone();
                    spawn_local(async move {
                        *status_copy.write() = super::freenet_synchronizer::SynchronizerStatus::Connected;
                    });
                    let _ = ready_tx.send(());
                }
            },
        );

        // Wait for connection with timeout
        info!("Waiting for connection with timeout of {}ms", CONNECTION_TIMEOUT_MS);
        match ready_rx.timeout(Duration::from_millis(CONNECTION_TIMEOUT_MS)).await {
            Ok(Ok(_)) => {
                info!("WebSocket connection established successfully");
                self.web_api = Some(web_api);
                self.connected = true;
                *self.synchronizer_status.write() = super::freenet_synchronizer::SynchronizerStatus::Connected;
                Ok(())
            },
            _ => {
                let error = SynchronizerError::WebSocketError("WebSocket connection failed or timed out".to_string());
                error!("{}", error);
                self.connected = false;
                *self.synchronizer_status.write() = super::freenet_synchronizer::SynchronizerStatus::Error(error.to_string());
                
                // Schedule reconnect
                let tx = message_tx.clone();
                spawn_local(async move {
                    info!("Scheduling reconnection attempt in {}ms", RECONNECT_INTERVAL_MS);
                    sleep(Duration::from_millis(RECONNECT_INTERVAL_MS)).await;
                    info!("Attempting reconnection now");
                    if let Err(e) = tx.unbounded_send(super::freenet_synchronizer::SynchronizerMessage::Connect) {
                        error!("Failed to send reconnect message: {}", e);
                    }
                });
                
                Err(error)
            }
        }
    }

    pub fn get_api(&self) -> Option<&WebApi> {
        if !self.connected {
            info!("get_api called but connection is not ready");
        }
        self.web_api.as_ref()
    }

    pub fn get_api_mut(&mut self) -> Option<&mut WebApi> {
        if !self.connected || self.web_api.is_none() {
            // Log that the API is not initialized
            error!("WebAPI is not initialized or connection is not ready");
        }
        self.web_api.as_mut()
    }

    pub fn set_api(&mut self, api: WebApi) {
        self.web_api = Some(api);
    }
    
    pub fn get_status_signal(&self) -> &Signal<super::freenet_synchronizer::SynchronizerStatus> {
        &self.synchronizer_status
    }
}
