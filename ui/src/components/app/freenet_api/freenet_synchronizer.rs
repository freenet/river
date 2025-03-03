use super::constants::*;
use super::sync_status::{SyncStatus, SYNC_STATUS};
use crate::invites::PendingInvites;
use crate::room_data::Rooms;
use crate::util::sleep;
use dioxus::prelude::*;
use dioxus::logger::tracing::{info, error};
use ed25519_dalek::VerifyingKey;
use std::collections::HashSet;
use std::time::Duration;
use wasm_bindgen_futures::spawn_local;
use freenet_stdlib::{
    client_api::WebApi,
    prelude::ContractKey,
};

pub struct FreenetSynchronizer {
    subscribed_contracts: HashSet<ContractKey>,
    is_connected: bool,
    rooms: Signal<Rooms>,
    pending_invites: Signal<PendingInvites>,
    sync_status: Signal<SyncStatus>,
    websocket: Option<web_sys::WebSocket>,
    web_api: Option<WebApi>,
}
impl FreenetSynchronizer {
    pub fn new(
        rooms: Signal<Rooms>,
        pending_invites: Signal<PendingInvites>,
        sync_status: Signal<SyncStatus>,
    ) -> Self {
        Self {
            subscribed_contracts: HashSet::new(),
            is_connected: false,
            rooms,
            pending_invites,
            sync_status,
            websocket: None,
            web_api: None,
        }
    }

    pub fn start(mut synchronizer: Signal<FreenetSynchronizer>) {
        info!("Starting FreenetSynchronizer");
        
        // Clone the signals we need for the effect
        let rooms_signal = {
            let sync = synchronizer.read();
            sync.rooms.clone()
        };
        
        let mut synchronizer_for_effect = synchronizer.clone();
        
        use_effect(move || {
            {
                let _rooms_snapshot = rooms_signal.read();
                info!("Rooms state changed, checking for sync needs");
            }
            
            // Process rooms when state changes
            {
                let mut sync = synchronizer_for_effect.write();
                sync.process_rooms();
            }
            
            (move || {
                info!("Rooms effect cleanup");
            })()
        });

        // Start connection
        {
            let synchronizer_for_connect = synchronizer.clone();
            let mut sync = synchronizer.write();
            sync.connect(synchronizer_for_connect);
        }
    }

    fn connect(&mut self, mut synchronizer_signal: Signal<FreenetSynchronizer>) {
        info!("Connecting to Freenet node at: {}", WEBSOCKET_URL);
        *SYNC_STATUS.write() = SyncStatus::Connecting;
        self.sync_status.set(SyncStatus::Connecting);

        let mut sync_status = self.sync_status.clone();

        spawn_local(async move {
            // Initialize connection
            let result = {
                let mut sync = synchronizer_signal.write();
                sync.initialize_connection().await
            };
            
            match result {
                Ok(_) => {
                    info!("Successfully connected to Freenet node");
                    {
                        let mut sync = synchronizer_signal.write();
                        sync.is_connected = true;
                        sync.process_rooms();
                    }
                    *SYNC_STATUS.write() = SyncStatus::Connected;
                    sync_status.set(SyncStatus::Connected);
                }
                Err(e) => {
                    error!("Failed to connect to Freenet node: {}", e);
                    *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                    sync_status.set(SyncStatus::Error(e));
                    
                    // Schedule reconnect
                    let synchronizer_for_reconnect = synchronizer_signal.clone();
                    spawn_local(async move {
                        sleep(Duration::from_millis(RECONNECT_INTERVAL_MS)).await;
                        let mut sync = synchronizer_signal.write();
                        sync.connect(synchronizer_for_reconnect);
                    });
                }
            }
        });
    }

    /// Process rooms that need synchronization
    fn process_rooms(&mut self) {
        info!("Processing rooms for synchronization");
        // This is a stub implementation - you'll need to implement the actual logic
    }

    /// Request room state for a given owner key
    pub async fn request_room_state(&mut self, owner_key: &VerifyingKey) -> Result<(), String> {
        info!("Requesting room state for owner: {:?}", owner_key);
        // This is a stub implementation - you'll need to implement the actual logic
        Ok(())
    }

    async fn initialize_connection(&mut self) -> Result<(), String> {
        let websocket = web_sys::WebSocket::new(WEBSOCKET_URL).map_err(|e| {
            let error_msg = format!("Failed to create WebSocket: {:?}", e);
            error!("{}", error_msg);
            error_msg
        })?;

        let (response_tx, _response_rx) = futures::channel::mpsc::unbounded();
        let (ready_tx, ready_rx) = futures::channel::oneshot::channel();

        let web_api = WebApi::start(
            websocket.clone(),
            move |result| {
                let sender = response_tx.clone();
                spawn_local(async move {
                    let mapped_result = result.map_err(|e| e.to_string());
                    if let Err(e) = sender.unbounded_send(mapped_result) {
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
                let _ = ready_tx.send(());
            },
        );

        let timeout = async {
            sleep(Duration::from_millis(CONNECTION_TIMEOUT_MS)).await;
            Err::<(), String>("WebSocket connection timed out".to_string())
        };

        match futures::future::select(Box::pin(ready_rx), Box::pin(timeout)).await {
            futures::future::Either::Left((Ok(_), _)) => {
                info!("WebSocket connection established successfully");
                self.websocket = Some(websocket);
                self.web_api = Some(web_api);
                Ok(())
            }
            _ => {
                let error_msg = "WebSocket connection failed or timed out".to_string();
                error!("{}", error_msg);
                Err(error_msg)
            }
        }
    }
}
