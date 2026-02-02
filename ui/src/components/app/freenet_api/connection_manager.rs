#[cfg(target_arch = "wasm32")]
mod imp {
    use crate::components::app::freenet_api::constants::*;
    use crate::components::app::freenet_api::error::SynchronizerError;
    use crate::components::app::freenet_api::freenet_synchronizer;
    use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
    use crate::components::app::{AUTH_TOKEN, SYNC_STATUS, WEB_API};
    use crate::util::sleep;
    use dioxus::logger::tracing::{error, info, warn};
    use dioxus::prelude::ReadableExt;
    use freenet_stdlib::client_api::{ClientError, HostResponse, WebApi};
    use futures::channel::mpsc::UnboundedSender;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use wasm_bindgen_futures::spawn_local;

    /// Error prefix sent by freenet-core when auth token is invalid/stale.
    /// This typically happens after a node restart when the in-memory token map is cleared.
    const AUTH_TOKEN_INVALID_ERROR: &str = "AUTH_TOKEN_INVALID";

    /// Guard to prevent multiple page reload tasks from being spawned.
    /// Once a reload is scheduled, subsequent AUTH_TOKEN_INVALID errors are ignored.
    static RELOAD_SCHEDULED: AtomicBool = AtomicBool::new(false);

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
            let base_url = get_websocket_url();
            let websocket_url = if let Some(token) = auth_token {
                if base_url.contains('?') {
                    format!("{}&authToken={}", base_url, token)
                } else {
                    format!("{}?authToken={}", base_url, token)
                }
            } else {
                base_url
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

            info!("Starting WebAPI with callbacks");

            // Clone message_tx for the error handler to trigger reconnection
            let error_tx = message_tx.clone();

            let web_api = WebApi::start(
                websocket.clone(),
                move |result: Result<HostResponse, ClientError>| {
                    // Check for AUTH_TOKEN_INVALID error - this means the node was restarted
                    // and we need to refresh the page to get a new valid token
                    if let Err(ref e) = result {
                        let error_str = e.to_string();
                        if error_str.contains(AUTH_TOKEN_INVALID_ERROR) {
                            // Guard against multiple reload tasks being spawned
                            if RELOAD_SCHEDULED.swap(true, Ordering::SeqCst) {
                                info!("Page reload already scheduled, ignoring duplicate AUTH_TOKEN_INVALID");
                                return;
                            }
                            warn!(
                                "Auth token is no longer valid (node may have restarted). \
                                 Scheduling page refresh to get a new token."
                            );
                            // Don't reload immediately - this would interrupt any pending
                            // async operations like delegate registration. Instead, schedule
                            // the reload with a small delay to allow current operations to
                            // complete or fail gracefully.
                            spawn_local(async move {
                                *SYNC_STATUS.write() =
                                    freenet_synchronizer::SynchronizerStatus::Error(
                                        "Authentication expired. Refreshing page...".to_string(),
                                    );
                                // Wait for pending delegate registration to complete.
                                // This is needed because set_up_chat_delegate() might be
                                // running concurrently, and we need its WebSocket messages
                                // to be sent before we trigger a page reload.
                                // 500ms should be enough for the messages to be queued and
                                // sent over the WebSocket.
                                sleep(Duration::from_millis(500)).await;
                                // Now trigger page refresh
                                if let Some(window) = web_sys::window() {
                                    if let Err(e) = window.location().reload() {
                                        error!("Failed to reload page: {:?}", e);
                                    }
                                }
                            });
                            return; // Don't process this error further
                        }
                    }

                    let mapped_result: Result<
                        freenet_stdlib::client_api::HostResponse,
                        SynchronizerError,
                    > = result.map_err(|e| SynchronizerError::WebSocketError(e.to_string()));
                    let tx = message_tx_clone.clone();
                    spawn_local(async move {
                        if let Err(e) = tx.unbounded_send(
                            freenet_synchronizer::SynchronizerMessage::ApiResponse(mapped_result),
                        ) {
                            error!("Failed to send API response: {}", e);
                        }
                    });
                },
                {
                    move |error| {
                        let error_msg = format!("WebSocket error: {}", error);
                        error!("{}", error_msg);

                        // Check if this is a connection closed error
                        let is_connection_closed = error_msg.contains("connection closed");

                        let tx = error_tx.clone();
                        spawn_local(async move {
                            *SYNC_STATUS.write() =
                                freenet_synchronizer::SynchronizerStatus::Error(error_msg);

                            // Trigger reconnection for connection closed errors
                            if is_connection_closed {
                                info!("Connection closed, triggering reconnection");
                                if let Err(e) = tx.unbounded_send(
                                    freenet_synchronizer::SynchronizerMessage::ConnectionLost,
                                ) {
                                    error!("Failed to send ConnectionLost message: {}", e);
                                }
                            }
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

            info!(
                "Waiting for connection with timeout of {}ms",
                CONNECTION_TIMEOUT_MS
            );

            let timeout_future = sleep(Duration::from_millis(CONNECTION_TIMEOUT_MS));

            let result =
                futures::future::select(Box::pin(ready_rx), Box::pin(timeout_future)).await;

            match result {
                futures::future::Either::Left((Ok(_), _)) => {
                    info!("WebSocket connection established successfully");
                    *WEB_API.write() = Some(web_api);
                    self.connected = true;
                    *SYNC_STATUS.write() = SynchronizerStatus::Connected;
                    Ok(())
                }
                _ => {
                    let error = SynchronizerError::WebSocketError(
                        "WebSocket connection failed or timed out".to_string(),
                    );
                    error!("{}", error);
                    *SYNC_STATUS.write() =
                        freenet_synchronizer::SynchronizerStatus::Error(error.to_string());

                    let ready_state = websocket.ready_state();
                    if ready_state == web_sys::WebSocket::CONNECTING
                        || ready_state == web_sys::WebSocket::OPEN
                    {
                        info!("Closing WebSocket due to connection failure");
                        if let Err(e) = websocket.close() {
                            error!("Failed to close WebSocket: {:?}", e);
                        }
                    }

                    Err(error)
                }
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use imp::ConnectionManager;

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use crate::components::app::freenet_api::error::SynchronizerError;
    use crate::components::app::freenet_api::freenet_synchronizer;
    use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
    use crate::components::app::{SYNC_STATUS, WEB_API};
    use dioxus::logger::tracing::warn;
    use futures::channel::mpsc::UnboundedSender;

    #[derive(Default)]
    pub struct ConnectionManager;

    impl ConnectionManager {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn is_connected(&self) -> bool {
            false
        }

        pub async fn initialize_connection(
            &mut self,
            message_tx: UnboundedSender<freenet_synchronizer::SynchronizerMessage>,
        ) -> Result<(), SynchronizerError> {
            let _ = message_tx;
            warn!("ConnectionManager::initialize_connection is a no-op on non-wasm targets");
            *SYNC_STATUS.write() = SynchronizerStatus::Error(
                "Web API connection only available when targeting wasm32".into(),
            );
            WEB_API.write().take();
            Err(SynchronizerError::WebSocketNotSupported(
                "River UI connection only available when targeting wasm32".into(),
            ))
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use imp::ConnectionManager;
