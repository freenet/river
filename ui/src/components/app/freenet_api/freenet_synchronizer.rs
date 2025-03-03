use super::constants::*;
use super::sync_status::{SyncStatus, SYNC_STATUS};
use crate::invites::PendingInvites;
use crate::room_data::{Rooms, RoomData, RoomSyncStatus};
use crate::util::sleep;
use dioxus::prelude::*;
use dioxus::logger::tracing::{info, error, debug, warn};
use ed25519_dalek::VerifyingKey;
use std::collections::{HashSet, HashMap};
use std::time::Duration;
use wasm_bindgen_futures::spawn_local;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use freenet_stdlib::{
    client_api::{WebApi, HostResponse, ApiRequest},
    prelude::{ContractKey, ContractState},
};
use freenet_stdlib::client_api::HostResponse;
use river_common::room_state::ChatRoomStateV1;

/// Manages synchronization between local room state and Freenet network
pub struct FreenetSynchronizer {
    subscribed_contracts: HashSet<ContractKey>,
    is_connected: bool,
    rooms: Signal<Rooms>,
    pending_invites: Signal<PendingInvites>,
    sync_status: Signal<SyncStatus>,
    websocket: Option<web_sys::WebSocket>,
    web_api: Option<WebApi>,
    pending_puts: HashMap<ContractKey, PendingOperation>,
}

/// Tracks operations in progress
enum PendingOperation {
    Put {
        timestamp: std::time::Instant,
        room_key: VerifyingKey,
    },
    Subscribe {
        timestamp: std::time::Instant,
        room_key: VerifyingKey,
    },
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
            pending_puts: HashMap::new(),
        }
    }
}

// Extension trait to add methods to Signal<FreenetSynchronizer>
pub trait FreenetSynchronizerExt {
    fn start(self);
    fn connect(&mut self);
    fn process_rooms(&mut self);
    fn request_room_state(&mut self, owner_key: &VerifyingKey) -> impl std::future::Future<Output = Result<(), String>>;
    fn put_room_state(&mut self, room_key: &VerifyingKey) -> impl std::future::Future<Output = Result<(), String>>;
    fn subscribe_to_room(&mut self, room_key: &VerifyingKey) -> impl std::future::Future<Output = Result<(), String>>;
}

impl FreenetSynchronizerExt for Signal<FreenetSynchronizer> {
    fn start(mut self) {
        info!("Starting FreenetSynchronizer");
        
        // Clone the signals we need for the effect
        let rooms_signal = {
            let sync = self.read();
            sync.rooms.clone()
        };
        
        let effect_signal = self.clone();
        
        use_effect(move || {
            {
                let _rooms_snapshot = rooms_signal.read();
                info!("Rooms state changed, checking for sync needs");
            }
            
            // Process rooms when state changes
            effect_signal.clone().process_rooms();
            
            (move || {
                info!("Rooms effect cleanup");
            })()
        });

        // Start connection
        self.connect();
    }

    fn connect(&mut self) {
        info!("Connecting to Freenet node at: {}", WEBSOCKET_URL);
        *SYNC_STATUS.write() = SyncStatus::Connecting;
        
        // Get the sync_status signal
        let mut sync_status = {
            let sync = self.read();
            sync.sync_status.clone()
        };
        sync_status.set(SyncStatus::Connecting);

        let mut signal_clone = self.clone();

        spawn_local(async move {
            // Initialize connection
            let result = initialize_connection(signal_clone.clone()).await;
            
            match result {
                Ok(response_rx) => {
                    info!("Successfully connected to Freenet node");
                    {
                        let mut sync = signal_clone.write();
                        sync.is_connected = true;
                    }
                    
                    // Start processing API responses
                    process_api_responses(signal_clone.clone(), response_rx);
                    
                    // Process rooms to sync them
                    signal_clone.process_rooms();
                    
                    *SYNC_STATUS.write() = SyncStatus::Connected;
                    sync_status.set(SyncStatus::Connected);
                }
                Err(e) => {
                    error!("Failed to connect to Freenet node: {}", e);
                    *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                    sync_status.set(SyncStatus::Error(e));
                    
                    // Schedule reconnect
                    let mut reconnect_signal = signal_clone.clone();
                    spawn_local(async move {
                        sleep(Duration::from_millis(RECONNECT_INTERVAL_MS)).await;
                        reconnect_signal.connect();
                    });
                }
            }
        });
    }

    fn process_rooms(&mut self) {
        let sync = self.write();
        info!("Processing rooms for synchronization");
        
        // Get a snapshot of the rooms
        let rooms = sync.rooms.read();
        
        // Iterate through rooms and check which ones need synchronization
        for (room_key, room_data) in rooms.map.iter() {
            match room_data.sync_status {
                RoomSyncStatus::NeedsPut => {
                    info!("Room needs PUT: {:?}", room_key);
                    // Clone for the async block
                    let room_key_clone = room_key.clone();
                    let mut self_clone = self.clone();
                    
                    // Schedule the PUT operation
                    spawn_local(async move {
                        match self_clone.put_room_state(&room_key_clone).await {
                            Ok(_) => info!("Successfully PUT room state for {:?}", room_key_clone),
                            Err(e) => error!("Failed to PUT room state: {}", e),
                        }
                    });
                },
                RoomSyncStatus::Subscribed => {
                    // Check if the room state has changed since last sync
                    if room_data.needs_sync() {
                        info!("Room needs sync: {:?}", room_key);
                        // Check if we're already subscribed
                    if !sync.subscribed_contracts.contains(&room_data.contract_key) {
                        // Clone for the async block
                        let room_key_clone = room_key.clone();
                        let mut self_clone = self.clone();
                        
                        // Schedule the subscribe operation
                        spawn_local(async move {
                            match self_clone.subscribe_to_room(&room_key_clone).await {
                                Ok(_) => info!("Successfully subscribed to room {:?}", room_key_clone),
                                Err(e) => error!("Failed to subscribe to room: {}", e),
                            }
                        });
                    }
                },
                },
                _ => {} // No action needed for other states
            }
        }
        
        drop(rooms); // Explicitly drop the read lock
    }

    async fn request_room_state(&mut self, owner_key: &VerifyingKey) -> Result<(), String> {
        let sync = self.write();
        info!("Requesting room state for owner: {:?}", owner_key);
        
        // Get the web_api
        let web_api = match &sync.web_api {
            Some(api) => api,
            None => return Err("WebAPI not initialized".to_string()),
        };
        
        // Create contract key from owner key
        let contract_key = ContractKey::from_verifying_key(owner_key);
        
        // Request the contract state
        web_api.get_contract_state(&contract_key)
            .map_err(|e| format!("Failed to request contract state: {}", e))?;
        
        Ok(())
    }
    
    async fn put_room_state(&mut self, room_key: &VerifyingKey) -> Result<(), String> {
        let mut sync = self.write();
        info!("Putting room state for: {:?}", room_key);
        
        // Get the web_api
        let web_api = match &sync.web_api {
            Some(api) => api,
            None => return Err("WebAPI not initialized".to_string()),
        };
        
        // Get the room data
        let mut rooms = sync.rooms.write();
        let room_data = match rooms.map.get_mut(room_key) {
            Some(data) => data,
            None => return Err(format!("Room not found: {:?}", room_key)),
        };
        
        // Serialize the room state
        let state = ContractState::from(&room_data.room_state);
        
        // Put the contract state
        web_api.put_contract_state(&room_data.contract_key, &state)
            .map_err(|e| format!("Failed to put contract state: {}", e))?;
        
        // Add to pending operations
        sync.pending_puts.insert(
            room_data.contract_key.clone(),
            PendingOperation::Put {
                timestamp: std::time::Instant::now(),
                room_key: room_key.clone(),
            }
        );
        
        // Update room sync status
        if room_data.sync_status == RoomSyncStatus::NeedsPut {
            room_data.sync_status = RoomSyncStatus::PutInProgress;
        }
        
        Ok(())
    }
    
    async fn subscribe_to_room(&mut self, room_key: &VerifyingKey) -> Result<(), String> {
        let mut sync = self.write();
        info!("Subscribing to room: {:?}", room_key);
        
        // Get the web_api
        let web_api = match &sync.web_api {
            Some(api) => api,
            None => return Err("WebAPI not initialized".to_string()),
        };
        
        // Get the room data
        let mut rooms = sync.rooms.write();
        let room_data = match rooms.map.get_mut(room_key) {
            Some(data) => data,
            None => return Err(format!("Room not found: {:?}", room_key)),
        };
        
        // Subscribe to the contract
        web_api.subscribe(&room_data.contract_key)
            .map_err(|e| format!("Failed to subscribe to contract: {}", e))?;
        
        // Add to pending operations
        sync.pending_puts.insert(
            room_data.contract_key.clone(),
            PendingOperation::Subscribe {
                timestamp: std::time::Instant::now(),
                room_key: room_key.clone(),
            }
        );
        
        // Update room sync status
        room_data.sync_status = RoomSyncStatus::SubscriptionInProgress;
        
        Ok(())
    }
}

/// Initializes the connection to the Freenet node
pub async fn initialize_connection(mut signal: Signal<FreenetSynchronizer>) -> Result<UnboundedReceiver<Result<HostResponse, String>>, String> {
    let websocket = web_sys::WebSocket::new(WEBSOCKET_URL).map_err(|e| {
        let error_msg = format!("Failed to create WebSocket: {:?}", e);
        error!("{}", error_msg);
        error_msg
    })?;

    // Create channel for API responses
    let (response_tx, response_rx) = futures::channel::mpsc::unbounded();
    let (ready_tx, ready_rx) = futures::channel::oneshot::channel();

    let web_api = WebApi::start(
        websocket.clone(),
        create_response_handler(response_tx.clone()),
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
            let mut sync = signal.write();
            sync.websocket = Some(websocket);
            sync.web_api = Some(web_api);
            Ok(response_rx)
        }
        _ => {
            let error_msg = "WebSocket connection failed or timed out".to_string();
            error!("{}", error_msg);
            Err(error_msg)
        }
    }
}

/// Creates a response handler function for the WebAPI
fn create_response_handler(sender: UnboundedSender<Result<HostResponse, String>>) -> impl Fn(Result<HostResponse, freenet_stdlib::client_api::Error>) {
    move |result| {
        let mapped_result = result.map_err(|e| e.to_string());
        spawn_local({
            let sender = sender.clone();
            async move {
                if let Err(e) = sender.unbounded_send(mapped_result) {
                    error!("Failed to send API response: {}", e);
                }
            }
        });
    }
}

/// Processes API responses from the Freenet node
fn process_api_responses(synchronizer: Signal<FreenetSynchronizer>, mut response_rx: UnboundedReceiver<Result<HostResponse, String>>) {
    spawn_local(async move {
        info!("Starting API response processor");
        
        while let Some(response) = response_rx.next().await {
            match response {
                Ok(api_response) => handle_api_response(synchronizer.clone(), api_response).await,
                Err(e) => error!("Error in API response: {}", e),
            }
        }
        
        warn!("API response stream ended");
    });
}

/// Handles individual API responses
async fn handle_api_response(mut synchronizer: Signal<FreenetSynchronizer>, response: HostResponse) {
    match response {
        HostResponse::ContractState { contract, state } => {
            info!("Received contract state for: {:?}", contract);
            update_room_state(synchronizer.clone(), contract, state).await;
        },
        HostResponse::ContractUpdate { contract, state } => {
            info!("Received contract update for: {:?}", contract);
            update_room_state(synchronizer.clone(), contract, state).await;
        },
        HostResponse::PutContractStateSuccess { contract } => {
            info!("Successfully put contract state for: {:?}", contract);
            handle_put_success(synchronizer.clone(), contract).await;
        },
        HostResponse::SubscribeSuccess { contract } => {
            info!("Successfully subscribed to contract: {:?}", contract);
            handle_subscribe_success(synchronizer.clone(), contract).await;
        },
        HostResponse::Error { request, error } => {
            error!("API error for request {:?}: {}", request, error);
            handle_api_error(synchronizer.clone(), request, error).await;
        },
        _ => {
            debug!("Unhandled API response: {:?}", response);
        }
    }
}

/// Updates room state from contract state
async fn update_room_state(mut synchronizer: Signal<FreenetSynchronizer>, contract: ContractKey, state: ContractState) {
    let mut sync = synchronizer.write();
    
    // Find the room with this contract key
    let mut rooms = sync.rooms.write();
    let room_key_opt = rooms.map.iter()
        .find(|(_, data)| data.contract_key == contract)
        .map(|(key, _)| key.clone());
    
    if let Some(room_key) = room_key_opt {
        if let Some(room_data) = rooms.map.get_mut(&room_key) {
            // Try to deserialize the state
            match ChatRoomStateV1::try_from(&state) {
                Ok(new_state) => {
                    info!("Updating room state for: {:?}", room_key);
                    room_data.room_state = new_state;
                    room_data.mark_synced();
                    
                    // Update sync status
                    if room_data.sync_status == RoomSyncStatus::SubscriptionInProgress {
                        room_data.sync_status = RoomSyncStatus::Subscribed;
                    }
                    
                    // Add to subscribed contracts if not already there
                    sync.subscribed_contracts.insert(contract);
                },
                Err(e) => {
                    error!("Failed to deserialize room state: {}", e);
                }
            }
        }
    } else {
        // This might be a response to a pending invitation
        handle_pending_invitation(sync, contract, state);
    }
}

/// Handles pending invitations when receiving contract state
fn handle_pending_invitation(mut sync: std::cell::RefMut<'_, FreenetSynchronizer>, contract: ContractKey, state: ContractState) {
    let mut pending_invites = sync.pending_invites.write();
    
    // Find any pending invitations for this contract
    for (owner_key, pending) in pending_invites.map.iter_mut() {
        let pending_contract = ContractKey::from_verifying_key(owner_key);
        
        if pending_contract == contract {
            info!("Found pending invitation for contract: {:?}", contract);
            
            // Try to deserialize the state
            match ChatRoomStateV1::try_from(&state) {
                Ok(room_state) => {
                    // Create a new room data entry
                    let room_data = RoomData {
                        owner_vk: owner_key.clone(),
                        room_state,
                        self_sk: pending.invitee_signing_key.clone(),
                        contract_key: contract.clone(),
                        sync_status: RoomSyncStatus::Subscribed,
                        last_synced_state: None,
                    };
                    
                    // Add to rooms
                    let mut rooms = sync.rooms.write();
                    rooms.map.insert(owner_key.clone(), room_data);
                    
                    // Update pending status
                    pending.status = crate::invites::PendingRoomStatus::Retrieved;
                    
                    // Add to subscribed contracts
                    sync.subscribed_contracts.insert(contract);
                    
                    info!("Successfully added room from invitation: {:?}", owner_key);
                },
                Err(e) => {
                    error!("Failed to deserialize room state for invitation: {}", e);
                    pending.status = crate::invites::PendingRoomStatus::Error(
                        format!("Failed to deserialize room state: {}", e)
                    );
                }
            }
            
            break;
        }
    }
}

/// Handles successful PUT operations
async fn handle_put_success(mut synchronizer: Signal<FreenetSynchronizer>, contract: ContractKey) {
    let mut sync = synchronizer.write();
    
    // Check if this was a pending operation
    if let Some(op) = sync.pending_puts.remove(&contract) {
        match op {
            PendingOperation::Put { room_key, .. } => {
                info!("PUT operation completed for room: {:?}", room_key);
                
                // Update room status
                let mut rooms = sync.rooms.write();
                if let Some(room_data) = rooms.map.get_mut(&room_key) {
                    if room_data.sync_status == RoomSyncStatus::PutInProgress {
                        room_data.sync_status = RoomSyncStatus::NeedsSync;
                        room_data.mark_synced();
                    }
                }
                
                // Schedule subscription after a short delay
                let mut sync_clone = synchronizer.clone();
                let room_key_clone = room_key.clone();
                spawn_local(async move {
                    // Wait a bit before subscribing to ensure the state is available
                    sleep(Duration::from_millis(1000)).await;
                    
                    match sync_clone.subscribe_to_room(&room_key_clone).await {
                        Ok(_) => info!("Scheduled subscription to room after PUT: {:?}", room_key_clone),
                        Err(e) => error!("Failed to subscribe after PUT: {}", e),
                    }
                });
            },
            _ => {}
        }
    }
}

/// Handles successful subscribe operations
async fn handle_subscribe_success(mut synchronizer: Signal<FreenetSynchronizer>, contract: ContractKey) {
    let mut sync = synchronizer.write();
    
    // Add to subscribed contracts
    sync.subscribed_contracts.insert(contract.clone());
    
    // Check if this was a pending operation
    if let Some(op) = sync.pending_puts.remove(&contract) {
        match op {
            PendingOperation::Subscribe { room_key, .. } => {
                info!("Subscribe operation completed for room: {:?}", room_key);
                
                // Update room status
                let mut rooms = sync.rooms.write();
                if let Some(room_data) = rooms.map.get_mut(&room_key) {
                    room_data.sync_status = RoomSyncStatus::Subscribed;
                }
            },
            _ => {}
        }
    }
}

/// Handles API errors
async fn handle_api_error(mut synchronizer: Signal<FreenetSynchronizer>, request: ApiRequest, error: String) {
    let mut sync = synchronizer.write();
    
    match request {
        ApiRequest::PutContractState { contract, .. } => {
            error!("Error putting contract state: {}", error);
            
            // Remove from pending operations
            if let Some(op) = sync.pending_puts.remove(&contract) {
                match op {
                    PendingOperation::Put { room_key, .. } => {
                        // Update room status
                        let mut rooms = sync.rooms.write();
                        if let Some(room_data) = rooms.map.get_mut(&room_key) {
                            room_data.sync_status = RoomSyncStatus::Error(error.clone());
                        }
                    },
                    _ => {}
                }
            }
        },
        ApiRequest::Subscribe { contract } => {
            error!("Error subscribing to contract: {}", error);
            
            // Remove from pending operations
            if let Some(op) = sync.pending_puts.remove(&contract) {
                match op {
                    PendingOperation::Subscribe { room_key, .. } => {
                        // Update room status
                        let mut rooms = sync.rooms.write();
                        if let Some(room_data) = rooms.map.get_mut(&room_key) {
                            room_data.sync_status = RoomSyncStatus::Error(error.clone());
                        }
                    },
                    _ => {}
                }
            }
        },
        ApiRequest::GetContractState { contract } => {
            error!("Error getting contract state: {}", error);
            
            // Check if this was for a pending invitation
            let mut pending_invites = sync.pending_invites.write();
            for (owner_key, pending) in pending_invites.map.iter_mut() {
                let pending_contract = ContractKey::from_verifying_key(owner_key);
                
                if pending_contract == contract {
                    pending.status = crate::invites::PendingRoomStatus::Error(
                        format!("Failed to get room state: {}", error)
                    );
                    break;
                }
            }
        },
        _ => {
            error!("Unhandled API error for request {:?}: {}", request, error);
        }
    }
}
