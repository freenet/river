use super::constants::*;
use super::sync_status::{SyncStatus, SYNC_STATUS};
use crate::constants::ROOM_CONTRACT_WASM;
use crate::invites::PendingInvites;
use crate::room_data::{RoomSyncStatus, Rooms};
use crate::components::app::room_state_handler;
use crate::util::{to_cbor_vec, sleep};
use dioxus::prelude::*;
use dioxus::logger::tracing::{info, error, warn};
use ed25519_dalek::VerifyingKey;
use futures::StreamExt;
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::collections::HashSet;
use std::time::Duration;
use wasm_bindgen_futures::spawn_local;
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi},
    prelude::{
        ContractCode, ContractInstanceId, ContractKey, Parameters, ContractContainer,
        WrappedState, RelatedContracts, UpdateData,
    },
};
use ciborium::from_reader;
use freenet_scaffold::ComposableState;

#[derive(Clone)]
pub struct FreenetSynchronizer {
    subscribed_contracts: HashSet<ContractKey>,
    is_connected: bool,
    rooms: Signal<Rooms>,
    pending_invites: Signal<PendingInvites>,
    sync_status: Signal<SyncStatus>,
    websocket: Option<web_sys::WebSocket>,
    web_api: Option<()>,
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

    pub fn start(&mut self) {
        info!("Starting FreenetSynchronizer");
        let _rooms = self.rooms.clone();
        let _pending_invites = self.pending_invites.clone();
        let _sync_status = self.sync_status.clone();
        let mut synchronizer = self.clone();

        use_effect(move || {
            {
                let _rooms_snapshot = synchronizer.rooms.read();
                info!("Rooms state changed, checking for sync needs");
            }
            synchronizer.process_rooms();
            move || {
                info!("Rooms effect cleanup");
            }
        });

        self.connect();
    }

    fn connect(&mut self) {
        info!("Connecting to Freenet node at: {}", WEBSOCKET_URL);
        *SYNC_STATUS.write() = SyncStatus::Connecting;
        self.sync_status.set(SyncStatus::Connecting);

        let mut sync_status = self.sync_status.clone();
        let mut synchronizer = self.clone();

        spawn_local(async move {
            match synchronizer.initialize_connection().await {
                Ok(_) => {
                    info!("Successfully connected to Freenet node");
                    synchronizer.is_connected = true;
                    *SYNC_STATUS.write() = SyncStatus::Connected;
                    sync_status.set(SyncStatus::Connected);
                    synchronizer.process_rooms();
                }
                Err(e) => {
                    error!("Failed to connect to Freenet node: {}", e);
                    *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                    sync_status.set(SyncStatus::Error(e));
                    let mut sync_clone = synchronizer.clone();
                    spawn_local(async move {
                        sleep(Duration::from_millis(RECONNECT_INTERVAL_MS)).await;
                        sync_clone.connect();
                    });
                }
            }
        });
    }

    async fn initialize_connection(&mut self) -> Result<(), String> {
        let websocket = web_sys::WebSocket::new(WEBSOCKET_URL).map_err(|e| {
            let error_msg = format!("Failed to create WebSocket: {:?}", e);
            error!("{}", error_msg);
            error_msg
        })?;

        let (response_tx, response_rx) = futures::channel::mpsc::unbounded();
        let (ready_tx, ready_rx) = futures::channel::oneshot::channel();

        let mut web_api = WebApi::start(
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
                self.web_api = Some(());
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
