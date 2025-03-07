use super::constants::*;
use super::error::SynchronizerError;
use dioxus::prelude::*;
use dioxus::logger::tracing::{info, error};
use std::time::Duration;
use wasm_bindgen_futures::spawn_local;
use futures::channel::mpsc::UnboundedSender;
use freenet_stdlib::client_api::WebApi;
use crate::util::sleep;

/// Manages the connection to the Freenet node
pub struct ConnectionManager {
    web_api: Option<WebApi>,
    synchronizer_status: Signal<super::freenet_synchronizer::SynchronizerStatus>,
}

impl ConnectionManager {
    pub fn new(
        synchronizer_status: Signal<super::freenet_synchronizer::SynchronizerStatus>,
    ) -> Self {
        Self {
            web_api: None,
            synchronizer_status,
        }
    }

    /// Initializes the connection to the Freenet node
    pub async fn initialize_connection(
        &mut self,
        message_tx: UnboundedSender<super::freenet_synchronizer::SynchronizerMessage>,
    ) -> Result<(), SynchronizerError> {
        info!("Connecting to Freenet node at: {}", WEBSOCKET_URL);
        *self.synchronizer_status.write() = super::freenet_synchronizer::SynchronizerStatus::Connecting;
        
        let websocket = web_sys::WebSocket::new(WEBSOCKET_URL).map_err(|e| {
            let error_msg = format!("Failed to create WebSocket: {:?}", e);
            error!("{}", error_msg);
            SynchronizerError::WebSocketError(error_msg)
        })?;

        // Create channel for API responses
        let (ready_tx, ready_rx) = futures::channel::oneshot::channel();
        let message_tx_clone = message_tx.clone();
        
        let mut sync_status = self.synchronizer_status.clone();

        let web_api = WebApi::start(
            websocket.clone(),
            move |result| {
                let mapped_result = result.map_err(|e| SynchronizerError::WebSocketError(e.to_string()));
                spawn_local({
                    let tx = message_tx_clone.clone();
                    async move {
                        if let Err(e) = tx.unbounded_send(super::freenet_synchronizer::SynchronizerMessage::ApiResponse(mapped_result)) {
                            error!("Failed to send API response: {}", e);
                        }
                    }
                });
            },
            move |error| {
                let error_msg = format!("WebSocket error: {}", error);
                error!("{}", error_msg);
                *sync_status.write() = super::freenet_synchronizer::SynchronizerStatus::Error(error_msg);
            },
            move || {
                info!("WebSocket connected successfully");
                *sync_status.write() = super::freenet_synchronizer::SynchronizerStatus::Connected;
                let _ = ready_tx.send(());
            },
        );

        let timeout = async {
            sleep(Duration::from_millis(CONNECTION_TIMEOUT_MS)).await;
            Err::<(), SynchronizerError>(SynchronizerError::ConnectionTimeout(CONNECTION_TIMEOUT_MS))
        };

        match futures::future::select(Box::pin(ready_rx), Box::pin(timeout)).await {
            futures::future::Either::Left((Ok(_), _)) => {
                info!("WebSocket connection established successfully");
                self.web_api = Some(web_api);
                *self.synchronizer_status.write() = super::freenet_synchronizer::SynchronizerStatus::Connected;
                
                Ok(())
            }
            _ => {
                let error = SynchronizerError::WebSocketError("WebSocket connection failed or timed out".to_string());
                error!("{}", error);
                *self.synchronizer_status.write() = super::freenet_synchronizer::SynchronizerStatus::Error(error.to_string());
                
                // Schedule reconnect
                let tx = message_tx.clone();
                spawn_local(async move {
                    sleep(Duration::from_millis(RECONNECT_INTERVAL_MS)).await;
                    if let Err(e) = tx.unbounded_send(super::freenet_synchronizer::SynchronizerMessage::Connect) {
                        error!("Failed to send reconnect message: {}", e);
                    }
                });
                
                Err(error)
            }
        }
    }

    pub fn get_api(&self) -> Option<&WebApi> {
        self.web_api.as_ref()
    }

    pub fn get_api_mut(&mut self) -> Option<&mut WebApi> {
        self.web_api.as_mut()
    }

    pub fn set_api(&mut self, api: WebApi) {
        self.web_api = Some(api);
    }
    
    pub fn get_status_signal(&self) -> &Signal<super::freenet_synchronizer::SynchronizerStatus> {
        &self.synchronizer_status
    }
}
