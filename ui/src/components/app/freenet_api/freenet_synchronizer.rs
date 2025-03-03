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

/// Manages synchronization of chat rooms with the Freenet network
///
/// Handles WebSocket communication, room subscriptions, and state updates.
#[derive(Clone)]
pub struct FreenetSynchronizer {
    /// Set of contract keys we're currently subscribed to
    subscribed_contracts: HashSet<ContractKey>,
    
    /// Flag indicating if WebSocket is ready
    is_connected: bool,
    
    /// Reference to the rooms signal
    rooms: Signal<Rooms>,
    
    /// Reference to pending invites signal
    pending_invites: Signal<PendingInvites>,
    
    /// Reference to the sync status signal
    sync_status: Signal<SyncStatus>,
    
    /// WebSocket connection
    websocket: Option<web_sys::WebSocket>,
    
    /// WebApi instance for communication - not cloneable, so we use Option<()>
    web_api: Option<()>,
}

impl FreenetSynchronizer {
    /// Creates a new FreenetSynchronizer
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
    
    /// Starts the Freenet synchronizer
    ///
    /// This initializes the WebSocket connection and sets up the effect
    /// that handles communication with the Freenet network
    pub fn start(&mut self) {
        info!("Starting FreenetSynchronizer");
        
        // Clone what we need for the async task
        let _rooms = self.rooms.clone();
        let _pending_invites = self.pending_invites.clone();
        let mut sync_status = self.sync_status.clone();
        let mut synchronizer = self.clone();
        
        // Set up the effect to monitor rooms changes
        use_effect(move || {
            // Read the rooms to track changes
            let rooms_snapshot = synchronizer.rooms.read();
            info!("Rooms state changed, checking for sync needs");
            
            // Process rooms that need synchronization
            synchronizer.process_rooms();
            
            // Return a cleanup function
            (|| {
                // This will run when the component is unmounted or before the effect runs again
                info!("Rooms effect cleanup");
            })()
        });
        
        // Initialize the WebSocket connection
        self.connect();
    }
    
    /// Establishes WebSocket connection to Freenet
    fn connect(&mut self) {
        info!("Connecting to Freenet node at: {}", WEBSOCKET_URL);
        
        // Update the status
        *SYNC_STATUS.write() = SyncStatus::Connecting;
        self.sync_status.set(SyncStatus::Connecting);
        
        // Clone what we need for the async task
        let _rooms = self.rooms.clone();
        let _pending_invites = self.pending_invites.clone();
        let mut sync_status = self.sync_status.clone();
        let mut synchronizer = self.clone();
        
        spawn_local(async move {
            match synchronizer.initialize_connection().await {
                Ok(_) => {
                    info!("Successfully connected to Freenet node");
                    synchronizer.is_connected = true;
                    *SYNC_STATUS.write() = SyncStatus::Connected;
                    sync_status.set(SyncStatus::Connected);
                    
                    // Process any rooms that need synchronization
                    synchronizer.process_rooms();
                },
                Err(e) => {
                    error!("Failed to connect to Freenet node: {}", e);
                    *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                    sync_status.set(SyncStatus::Error(e));
                    
                    // Schedule reconnection attempt
                    let mut sync_clone = synchronizer.clone();
                    spawn_local(async move {
                        sleep(Duration::from_millis(RECONNECT_INTERVAL_MS)).await;
                        sync_clone.connect();
                    });
                    }
                }
            }
        );
    }
    
    /// Initialize WebSocket connection to Freenet
    async fn initialize_connection(&mut self) -> Result<(), String> {
        // Create WebSocket
        let websocket = match web_sys::WebSocket::new(WEBSOCKET_URL) {
            Ok(ws) => {
                info!("WebSocket created successfully");
                ws
            },
            Err(e) => {
                let error_msg = format!("Failed to create WebSocket: {:?}", e);
                error!("{}", error_msg);
                return Err(error_msg);
            }
        };
        
        // Create a channel for host responses
        let (response_tx, _response_rx) = futures::channel::mpsc::unbounded();
        
        // Create a promise for connection readiness
        let (ready_tx, ready_rx) = futures::channel::oneshot::channel();
        
        // Set up WebApi
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
        
        // Wait for connection or timeout
        let timeout = async {
            sleep(Duration::from_millis(CONNECTION_TIMEOUT_MS)).await;
            Err::<(), String>("WebSocket connection timed out".to_string())
        };
        
        let connection_result = futures::future::select(
            Box::pin(ready_rx),
            Box::pin(timeout)
        ).await;
        
        match connection_result {
            futures::future::Either::Left((Ok(_), _)) => {
                info!("WebSocket connection established successfully");
                
                // Store the WebSocket
                self.websocket = Some(websocket.clone());
                self.web_api = Some(());
                
                // Clone what we need for the response handler
                let rooms = self.rooms.clone();
                let pending_invites = self.pending_invites.clone();
                let sync_status = self.sync_status.clone();
                let mut synchronizer = self.clone();
                
                // Start a task to handle responses
                spawn_local(async move {
                    while let Some(response) = response_rx.next().await {
                        match response {
                            Ok(host_response) => {
                                synchronizer.handle_host_response(host_response).await;
                            },
                            Err(e) => {
                                error!("Error from host response: {}", e);
                                *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                                sync_status.set(SyncStatus::Error(e));
                                break;
                            }
                        }
                    }
                    
                    // If we get here, the response channel has closed
                    error!("Response channel closed, attempting to reconnect");
                    synchronizer.connect();
                });
                
                Ok(())
            },
            _ => {
                let error_msg = "WebSocket connection failed or timed out".to_string();
                error!("{}", error_msg);
                Err(error_msg)
            }
        }
    }
    
    /// Handle a response from the Freenet host
    async fn handle_host_response(&mut self, response: HostResponse) {
        match response {
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::GetResponse { key, state, .. } => {
                        self.process_get_response(key, state.to_vec()).await;
                    },
                    ContractResponse::UpdateNotification { key, update } => {
                        self.process_update_notification(key, update).await;
                    },
                    ContractResponse::PutResponse { key } => {
                        self.process_put_response(key).await;
                    },
                    _ => {}
                }
            },
            HostResponse::Ok => {
                info!("Received OK response from host");
                *SYNC_STATUS.write() = SyncStatus::Connected;
                self.sync_status.set(SyncStatus::Connected);
                
                // Update room statuses
                let mut rooms = self.rooms.write();
                for room in rooms.map.values_mut() {
                    if matches!(room.sync_status, RoomSyncStatus::Subscribing) {
                        info!("Room subscription confirmed for: {:?}", room.owner_vk);
                        room.sync_status = RoomSyncStatus::Subscribed;
                        room.mark_synced();
                    }
                }
            },
            _ => {}
        }
    }
    
    /// Process a GET response from the Freenet network
    async fn process_get_response(&mut self, key: ContractKey, state: Vec<u8>) {
        info!("Received GetResponse for key: {:?}", key);
        info!("Response state size: {} bytes", state.len());
        
        if let Ok(room_state) = from_reader::<ChatRoomStateV1, &[u8]>(state.as_ref()) {
            info!("Successfully deserialized room state");
            
            let key_bytes: [u8; 32] = match key.id().as_bytes().try_into() {
                Ok(bytes) => bytes,
                Err(e) => {
                    error!("Invalid key length: {:?}", e);
                    return;
                }
            };
            
            match VerifyingKey::from_bytes(&key_bytes) {
                Ok(room_owner) => {
                    info!("Identified room owner from key: {:?}", room_owner);
                    let mut rooms_write = self.rooms.write();
                    let mut pending_write = self.pending_invites.write();
                    
                    let was_pending = room_state_handler::process_room_state_response(
                        &mut rooms_write,
                        &room_owner,
                        room_state.clone(),
                        key,
                        &mut pending_write,
                    );
                    
                    if was_pending {
                        info!("Processed pending invitation for room owned by: {:?}", room_owner);
                    } else {
                        // Regular room state update
                        info!("Processing regular room state update");
                        if let Some(room_data) = rooms_write.map.values_mut().find(|r| r.contract_key == key) {
                            let current_state = room_data.room_state.clone();
                            if let Err(e) = room_data.room_state.merge(
                                &current_state,
                                &room_data.parameters(),
                                &room_state,
                            ) {
                                error!("Failed to merge room state: {}", e);
                                *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                                self.sync_status.set(SyncStatus::Error(e.clone()));
                                room_data.sync_status = RoomSyncStatus::Error(e);
                            } else {
                                // Mark as synced after successful merge
                                room_data.mark_synced();
                            }
                        }
                    }
                },
                Err(e) => {
                    error!("Failed to convert key to VerifyingKey: {:?}", e);
                    
                    // Try to find the room by contract key in pending invites
                    let mut found = false;
                    // Create a scope to limit the lifetime of the read borrow
                    {
                        let pending_read = self.pending_invites.read();
                    
                    for (owner_vk, _) in pending_read.map.iter() {
                        let params = ChatRoomParametersV1 { owner: *owner_vk };
                        let params_bytes = to_cbor_vec(&params);
                        let parameters = Parameters::from(params_bytes);
                        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
                        let instance_id = ContractInstanceId::from_params_and_code(parameters, contract_code);
                        let computed_key = ContractKey::from(instance_id);
                        
                        if computed_key == key {
                            info!("Found matching pending invitation for key: {:?}", key);
                            
                            let mut rooms_write = self.rooms.write();
                            let mut pending_write = self.pending_invites.write();
                            
                            let was_pending = room_state_handler::process_room_state_response(
                                &mut rooms_write,
                                owner_vk,
                                room_state.clone(),
                                key,
                                &mut pending_write,
                            );
                            
                            if was_pending {
                                info!("Successfully processed pending invitation using alternative method");
                                found = true;
                                break;
                            }
                        }
                    }
                    
                    if !found {
                        error!("Could not find matching room or pending invitation for key: {:?}", key);
                    }
                }
            }
        }
        } else {
            error!("Failed to decode room state from bytes");
        }
    }
    
    /// Process an update notification from the Freenet network
    async fn process_update_notification(&mut self, key: ContractKey, update: UpdateData<'_>) {
        info!("Received UpdateNotification for key: {:?}", key);
        
        let mut rooms = self.rooms.write();
        
        // First try to find the room by owner key
        let key_bytes: [u8; 32] = match key.id().as_bytes().try_into() {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Invalid key length: {:?}", e);
                return;
            }
        };
        
        let room_data = match VerifyingKey::from_bytes(&key_bytes) {
            Ok(verifying_key) => {
                info!("Successfully converted key to VerifyingKey: {:?}", verifying_key);
                rooms.map.get_mut(&verifying_key)
            },
            Err(e) => {
                error!("Failed to convert key to VerifyingKey: {:?}", e);
                // Try to find by contract key instead
                rooms.map.values_mut().find(|r| r.contract_key == key)
            }
        };
        
        if let Some(room_data) = room_data {
            info!("Found matching room for update notification");
            
            match from_reader(update.unwrap_delta().as_ref()) {
                Ok(delta) => {
                    info!("Successfully deserialized delta");
                    let current_state = room_data.room_state.clone();
                    if let Err(e) = room_data.room_state.apply_delta(
                        &current_state,
                        &room_data.parameters(),
                        &Some(delta),
                    ) {
                        error!("Failed to apply delta: {}", e);
                        *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                        self.sync_status.set(SyncStatus::Error(e.clone()));
                        room_data.sync_status = RoomSyncStatus::Error(e);
                    } else {
                        info!("Successfully applied delta update to room state");
                        // Mark as synced after successful delta application
                        room_data.mark_synced();
                    }
                },
                Err(e) => {
                    error!("Failed to deserialize delta: {:?}", e);
                }
            }
        } else {
            error!("No matching room found for update notification with key: {:?}", key);
        }
    }
    
    /// Process a PUT response from the Freenet network
    async fn process_put_response(&mut self, key: ContractKey) {
        info!("Received PutResponse for key: {:?}", key);
        
        let mut rooms = self.rooms.write();
        
        // Find the room with this contract key
        if let Some(room) = rooms.map.values_mut().find(|r| r.contract_key == key) {
            info!("Room PUT confirmed for: {:?}", room.owner_vk);
            
            if matches!(room.sync_status, RoomSyncStatus::Putting) {
                room.sync_status = RoomSyncStatus::Unsubscribed;
                
                // After a successful PUT, we need to send the state update
                let owner_vk = room.owner_vk;
                let _contract_key = room.contract_key;
                let state_bytes = to_cbor_vec(&room.room_state);
                
                // Drop the rooms lock before spawning the task
                drop(rooms);
                
                // Clone what we need for the async task
                let synchronizer = self.clone();
                
                // Spawn a task to send the update after a delay
                spawn_local(async move {
                    // Wait after PUT to ensure contract is fully registered
                    info!("Delaying state update after successful PUT to allow contract registration ({} ms)", POST_PUT_DELAY_MS);
                    sleep(Duration::from_millis(POST_PUT_DELAY_MS)).await;
                    info!("Delay complete, proceeding with state update for room owned by: {:?}", owner_vk);
                    
                    // Send the update
                    if let Err(e) = synchronizer.send_update_request(contract_key, state_bytes).await {
                        error!("Failed to send room update after PUT: {}", e);
                    }
                    
                    // Subscribe to the room
                    if let Err(e) = synchronizer.send_subscribe_request(contract_key).await {
                        error!("Failed to subscribe to room after PUT: {}", e);
                    }
                });
            }
        } else {
            warn!("Received PUT response for unknown room with key: {:?}", key);
        }
    }
    
    /// Process rooms that need synchronization
    fn process_rooms(&mut self) {
        if !self.is_connected {
            info!("Not processing rooms because WebSocket is not connected");
            return;
        }
        
        let rooms_snapshot = self.rooms.read();
        info!("Processing {} rooms for synchronization", rooms_snapshot.map.len());
        
        for (owner_vk, room) in rooms_snapshot.map.iter() {
            // Handle rooms that need to be PUT
            if matches!(room.sync_status, RoomSyncStatus::NeedsPut) {
                info!("Found room that needs to be PUT with owner: {:?}", owner_vk);
                
                // Clone what we need for the async task
                let synchronizer = self.clone();
                let owner_key = *owner_vk;
                let contract_key = room.contract_key;
                let room_state = room.room_state.clone();
                let mut rooms = self.rooms.clone();
                
                spawn_local(async move {
                    // Update status to Putting
                    {
                        let mut rooms_write = rooms.write();
                        if let Some(room) = rooms_write.map.get_mut(&owner_key) {
                            room.sync_status = RoomSyncStatus::Putting;
                        }
                    }
                    
                    // Prepare the PUT request
                    let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
                    let parameters = ChatRoomParametersV1 { owner: owner_key };
                    let params_bytes = to_cbor_vec(&parameters);
                    let parameters_obj = Parameters::from(params_bytes);
                    
                    let contract_container = ContractContainer::from(
                        freenet_stdlib::prelude::ContractWasmAPIVersion::V1(
                            freenet_stdlib::prelude::WrappedContract::new(
                                std::sync::Arc::new(contract_code.clone()),
                                parameters_obj.clone(),
                            ),
                        )
                    );
                    
                    let state_bytes = to_cbor_vec(&room_state);
                    let wrapped_state = WrappedState::new(state_bytes);
                    
                    let put_request = ContractRequest::Put {
                        contract: contract_container,
                        state: wrapped_state,
                        related_contracts: RelatedContracts::default(),
                    };
                    
                    // Send the PUT request
                    if let Err(e) = synchronizer.send_request(put_request.into()).await {
                        error!("Failed to PUT room: {}", e);
                        
                        // Update room status to error
                        let mut rooms_write = rooms.write();
                        if let Some(room) = rooms_write.map.get_mut(&owner_key) {
                            room.sync_status = RoomSyncStatus::Error(format!("Failed to PUT room: {}", e));
                        }
                    }
                });
            }
            // Subscribe if Unsubscribed
            else if matches!(room.sync_status, RoomSyncStatus::Unsubscribed) {
                info!("Found unsubscribed room with owner: {:?}", owner_vk);
                
                // Clone what we need for the async task
                let synchronizer = self.clone();
                let owner_key = *owner_vk;
                let contract_key = room.contract_key;
                let mut rooms = self.rooms.clone();
                
                spawn_local(async move {
                    // Update status to Subscribing
                    {
                        let mut rooms_write = rooms.write();
                        if let Some(room) = rooms_write.map.get_mut(&owner_key) {
                            room.sync_status = RoomSyncStatus::Subscribing;
                        }
                    }
                    
                    // Send the subscribe request
                    if let Err(e) = synchronizer.send_subscribe_request(contract_key).await {
                        error!("Failed to subscribe to room: {}", e);
                        
                        // Update room status to error
                        let mut rooms_write = rooms.write();
                        if let Some(room) = rooms_write.map.get_mut(&owner_key) {
                            room.sync_status = RoomSyncStatus::Error(format!("Failed to subscribe to room: {}", e));
                        }
                    }
                });
            }
            // Check if room needs synchronization based on state comparison
            else if room.needs_sync() {
                info!("Found room that needs state synchronization with owner: {:?}", owner_vk);
                
                // Clone what we need for the async task
                let synchronizer = self.clone();
                let owner_key = *owner_vk;
                let contract_key = room.contract_key;
                let room_state = room.room_state.clone();
                let mut rooms = self.rooms.clone();
                
                spawn_local(async move {
                    // Mark the room as synced before sending the update
                    {
                        let mut rooms_write = rooms.write();
                        if let Some(room) = rooms_write.map.get_mut(&owner_key) {
                            room.mark_synced();
                        }
                    }
                    
                    // Send the update
                    let state_bytes = to_cbor_vec(&room_state);
                    if let Err(e) = synchronizer.send_update_request(contract_key, state_bytes).await {
                        error!("Failed to send room state update: {}", e);
                    }
                });
            }
        }
    }
    
    /// Send a request to the Freenet API
    async fn send_request(&self, request: ClientRequest<'static>) -> Result<(), String> {
        if !self.is_connected {
            return Err("WebSocket not connected".to_string());
        }
        
        // Create a new WebSocket connection for this request
        let websocket = match web_sys::WebSocket::new(WEBSOCKET_URL) {
            Ok(ws) => ws,
            Err(e) => return Err(format!("Failed to create WebSocket: {:?}", e)),
        };
        
        // Create a channel for the response
        let (response_tx, mut response_rx) = futures::channel::mpsc::unbounded();
        let (ready_tx, ready_rx) = futures::channel::oneshot::channel();
        
        // Set up WebApi
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
                error!("WebSocket error: {}", error);
            },
            move || {
                let _ = ready_tx.send(());
            },
        );
        
        // Wait for connection or timeout
        let timeout = async {
            sleep(Duration::from_millis(CONNECTION_TIMEOUT_MS)).await;
            Err::<(), _>("WebSocket connection timed out".to_string())
        };
        
        let connection_result = futures::future::select(
            Box::pin(ready_rx),
            Box::pin(timeout)
        ).await;
        
        match connection_result {
            futures::future::Either::Left((Ok(_), _)) => {
                let mut retries = 0;
                while retries < MAX_REQUEST_RETRIES {
                    match web_api.send(request.clone()).await {
                        Ok(_) => {
                            return Ok(());
                        },
                        Err(e) => {
                            let error_msg = format!("Failed to send request (attempt {}/{}): {}", 
                                                retries + 1, MAX_REQUEST_RETRIES, e);
                            error!("{}", error_msg);
                            
                            if retries == MAX_REQUEST_RETRIES - 1 {
                                return Err(error_msg);
                            }
                            
                            retries += 1;
                            sleep(Duration::from_millis(500)).await;
                        }
                    }
                }
                
                Err("Failed to send request after maximum retries".to_string())
            },
            _ => {
                Err("WebSocket connection failed or timed out".to_string())
            }
        }
    }
    
    /// Send a subscribe request to the Freenet API
    async fn send_subscribe_request(&self, key: ContractKey) -> Result<(), String> {
        info!("Sending subscribe request for key: {:?}", key);
        
        let subscribe_request = ContractRequest::Subscribe {
            key,
            summary: None,
        };
        
        self.send_request(subscribe_request.into()).await
    }
    
    /// Send an update request to the Freenet API
    async fn send_update_request(&self, key: ContractKey, state_bytes: Vec<u8>) -> Result<(), String> {
        info!("Sending update request for key: {:?}", key);
        info!("Update size: {} bytes", state_bytes.len());
        
        let update_request = ContractRequest::Update {
            key,
            data: UpdateData::State(state_bytes.into()),
        };
        
        self.send_request(update_request.into()).await
    }
    
    /// Requests room state for a specific room
    pub async fn request_room_state(&self, room_owner: &VerifyingKey) -> Result<(), String> {
        info!("Requesting room state for room owned by {:?}", room_owner);
        
        if !self.is_connected {
            return Err("WebSocket not connected".to_string());
        }
        
        // Prepare chat room parameters
        let parameters = ChatRoomParametersV1 { owner: *room_owner };
        let params_bytes = to_cbor_vec(&parameters);
        let parameters_obj = Parameters::from(params_bytes);
        
        // Generate contract key
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let instance_id = ContractInstanceId::from_params_and_code(parameters_obj, contract_code);
        let contract_key = ContractKey::from(instance_id);
        
        // Create get request
        let get_request = ContractRequest::Get {
            key: contract_key,
            return_contract_code: false
        };
        
        // Send the request
        self.send_request(get_request.into()).await
    }
}
