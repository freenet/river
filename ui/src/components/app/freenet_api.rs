//! Freenet API integration for chat room synchronization
//!
//! Handles WebSocket communication with Freenet network, manages room subscriptions,
//! and processes state updates.

use crate::invites::PendingInvites;
use crate::room_data::RoomSyncStatus;
use river_common::room_state::ChatRoomStateV1;
use dioxus::logger::tracing::{debug, info, error};
use dioxus::prelude::Readable;
use crate::{constants::ROOM_CONTRACT_WASM, room_data::Rooms, util::{to_cbor_vec, sleep}};
use std::time::Duration;
use dioxus::prelude::{
    use_context, use_coroutine, use_effect, Global, GlobalSignal, Signal, UnboundedSender, Writable,
};
use ed25519_dalek::VerifyingKey;
use freenet_scaffold::ComposableState;
use freenet_stdlib::client_api::WebApi;
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse},
    prelude::{ContractCode, ContractInstanceId, ContractKey, Parameters, ContractContainer, WrappedState, RelatedContracts},
};
use futures::StreamExt;
use river_common::room_state::ChatRoomParametersV1;
use std::collections::HashSet;

/// Represents the current synchronization status with the Freenet network
#[derive(Clone, Debug)]
pub enum SyncStatus {
    /// Attempting to establish connection
    Connecting,
    /// Successfully connected to Freenet
    Connected,
    /// Actively synchronizing room state
    Syncing,
    /// Error state with associated message
    Error(String),
}

use futures::sink::SinkExt;

/// Global signal tracking the current sync status
pub static SYNC_STATUS: GlobalSignal<SyncStatus> = Global::new(|| SyncStatus::Connecting);

/// WebSocket URL for connecting to local Freenet node
const WEBSOCKET_URL: &str = "ws://localhost:50509/v1/contract/command?encodingProtocol=native";

/// Sender handle for making requests to the Freenet API
#[derive(Clone)]
pub struct FreenetApiSender {
    /// Channel sender for client requests
    request_sender: UnboundedSender<ClientRequest<'static>>,
}

impl std::fmt::Debug for FreenetApiSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FreenetApiSender")
            .field("request_sender", &"<UnboundedSender>")
            .finish()
    }
}

/// Manages synchronization of chat rooms with the Freenet network
///
/// Handles WebSocket communication, room subscriptions, and state updates.
#[derive(Clone)]
pub struct FreenetApiSynchronizer {
    /// Set of contract keys we're currently subscribed to
    pub subscribed_contracts: HashSet<ContractKey>,

    /// Sender handle for making requests
    pub sender: FreenetApiSender,

    /// Flag indicating if WebSocket is ready
    #[allow(dead_code)]
    ws_ready: bool,
    
    /// Signal for rooms data
    rooms_signal: Option<Signal<Rooms>>,
    
    /// Signal for sync status
    status_signal: Option<Signal<SyncStatus>>,
    
    /// Signal for pending invites
    pending_invites_signal: Option<Signal<PendingInvites>>,
}

impl FreenetApiSynchronizer {
    /// Creates a new FreenetApiSynchronizer without starting it
    ///
    /// # Returns
    /// New instance of FreenetApiSynchronizer with:
    /// - Empty subscription set
    /// - Request sender initialized
    pub fn new() -> Self {
        let subscribed_contracts = HashSet::new();
        let (request_sender, _request_receiver) = futures::channel::mpsc::unbounded();
        let sender_for_struct = request_sender.clone();

        Self {
            subscribed_contracts,
            sender: FreenetApiSender {
                request_sender: sender_for_struct,
            },
            ws_ready: false,
            rooms_signal: None,
            status_signal: None,
            pending_invites_signal: None,
        }
    }
    
    /// Sets the signals needed for synchronization
    pub fn with_signals(
        mut self,
        rooms: Signal<Rooms>,
        status: Signal<SyncStatus>,
        pending_invites: Signal<PendingInvites>,
    ) -> Self {
        self.rooms_signal = Some(rooms);
        self.status_signal = Some(status);
        self.pending_invites_signal = Some(pending_invites);
        self
    }

    /// Initialize WebSocket connection to Freenet
    async fn initialize_connection() -> Result<(web_sys::WebSocket, WebApi), String> {
        info!("Starting FreenetApiSynchronizer...");
        // Update the global status
        *SYNC_STATUS.write() = SyncStatus::Connecting;
        
        // Also update the context signal if available
        if let Ok(mut status) = use_context::<Signal<SyncStatus>>().try_write() {
            *status = SyncStatus::Connecting;
        }

        let websocket_connection = match web_sys::WebSocket::new(WEBSOCKET_URL) {
            Ok(ws) => {
                info!("WebSocket created successfully");
                ws
            },
            Err(e) => {
                let error_msg = format!("Failed to connect to WebSocket: {:?}", e);
                error!("{}", error_msg);
                *SYNC_STATUS.write() = SyncStatus::Error(error_msg.clone());
                if let Ok(mut status) = use_context::<Signal<SyncStatus>>().try_write() {
                    *status = SyncStatus::Error(error_msg.clone());
                }
                return Err(error_msg);
            }
        };

        let (host_response_sender, _host_response_receiver) =
            futures::channel::mpsc::unbounded::<Result<freenet_stdlib::client_api::HostResponse, String>>();

        // Create a shared flag to track connection readiness
        // Use a thread-local AtomicBool to avoid lifetime issues with async blocks
        thread_local! {
            static IS_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        }
        
        // Helper function to access the AtomicBool safely
        let set_ready = || IS_READY.with(|flag| flag.store(true, std::sync::atomic::Ordering::SeqCst));
        let check_ready = || IS_READY.with(|flag| flag.load(std::sync::atomic::Ordering::SeqCst));

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
                if let Ok(mut status) = use_context::<Signal<SyncStatus>>().try_write() {
                    *status = SyncStatus::Connected;
                }
                // Signal that the connection is ready
                set_ready();
            },
        );

        // Wait for the connection to be ready or timeout
        let timeout_promise = async {
            sleep(Duration::from_millis(5000)).await;
            false
        };
        
        let check_ready = async {
            let mut attempts = 0;
            while attempts < 50 {  // Check for 5 seconds (50 * 100ms)
                if check_ready() {
                    return true;
                }
                sleep(Duration::from_millis(100)).await;
                attempts += 1;
            }
            false
        };
        
        // Store the result in a variable to avoid lifetime issues
        let select_result = futures::future::select(
            Box::pin(check_ready),
            Box::pin(timeout_promise)
        ).await;
        
        match select_result {
            futures::future::Either::Left((true, _)) => {
                info!("WebSocket connection established successfully");
                Ok((websocket_connection, web_api))
            },
            futures::future::Either::Left((false, _)) => {
                // This case happens when check_ready completes but returns false
                let error_msg = "WebSocket connection ready check failed".to_string();
                error!("{}", error_msg);
                *SYNC_STATUS.write() = SyncStatus::Error(error_msg.clone());
                Err(error_msg)
            },
            futures::future::Either::Right((_, _)) => {
                let error_msg = "WebSocket connection timed out".to_string();
                error!("{}", error_msg);
                *SYNC_STATUS.write() = SyncStatus::Error(error_msg.clone());
                Err(error_msg)
            }
        }
    }

    /// Process a GetResponse from the Freenet network
    fn process_get_response(
        &self,
        key: ContractKey, 
        state: Vec<u8>
    ) {
        info!("Received GetResponse for key: {:?}", key);
        debug!("Response state size: {} bytes", state.len());

        // Check if we have the required signals
        if self.rooms_signal.is_none() || self.pending_invites_signal.is_none() {
            error!("Cannot process GetResponse: required signals not available");
            return;
        }
        
        let mut rooms = self.rooms_signal.as_ref().unwrap().clone();
        let mut pending_invites = self.pending_invites_signal.as_ref().unwrap().clone();
        
        // Update rooms with received state
        if let Ok(room_state) = ciborium::from_reader::<ChatRoomStateV1, &[u8]>(state.as_ref()) {
            debug!("Successfully deserialized room state");

            // Try to find the room owner from the key
            let key_bytes: [u8; 32] = key.id().as_bytes().try_into().expect("Invalid key length");
            if let Ok(room_owner) = VerifyingKey::from_bytes(&key_bytes) {
                info!("Identified room owner from key: {:?}", room_owner);
                
                if let Some(rooms) = &self.rooms_signal {
                    if let Some(pending_invites) = &self.pending_invites_signal {
                        let rooms = rooms.clone();
                        let pending_invites = pending_invites.clone();
                        
                        if let Ok(mut rooms_write) = rooms.try_write() {
                            if let Ok(mut pending_write) = pending_invites.try_write() {
                        // Check if this is a pending invitation
                        debug!("Checking if this is a pending invitation");
                        let was_pending = crate::components::app::room_state_handler::process_room_state_response(
                            &mut rooms_write,
                            &room_owner,
                            room_state.clone(),
                            key,
                            &mut pending_write
                        );

                        if was_pending {
                            info!("Processed pending invitation for room owned by: {:?}", room_owner);
                        }

                        if !was_pending {
                            // Regular room state update
                            info!("Processing regular room state update");
                            if let Some(room_data) = rooms_write.map.values_mut().find(|r| r.contract_key == key) {
                                let current_state = room_data.room_state.clone();
                                if let Err(e) = room_data.room_state.merge(
                                    &current_state,
                                    &room_data.parameters(),
                                    &room_state
                                ) {
                                    error!("Failed to merge room state: {}", e);
                                    *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                                    room_data.sync_status = RoomSyncStatus::Error(e);
                                }
                            }
                        }
                    }
                }
                }
            } else {
                error!("Failed to convert key to VerifyingKey");
            }
        } else {
            error!("Failed to decode room state from bytes: {:?}", state.as_slice());
        }
    }

    /// Process an UpdateNotification from the Freenet network
    fn process_update_notification(&self, key: ContractKey, update: freenet_stdlib::prelude::UpdateData) {
        info!("Received UpdateNotification for key: {:?}", key);
        
        // Check if we have the rooms signal
        if self.rooms_signal.is_none() {
            error!("Cannot process UpdateNotification: rooms signal not available");
            return;
        }
        
        if let Some(rooms) = &self.rooms_signal {
            let rooms = rooms.clone();
            if let Ok(mut rooms_write) = rooms.try_write() {
                // Process the update notification
                let key_bytes: [u8; 32] = key.id().as_bytes().try_into().expect("Invalid key length");
                if let Some(room_data) = rooms_write.map.get_mut(&VerifyingKey::from_bytes(&key_bytes).expect("Invalid key bytes")) {
                    debug!("Processing delta update for room");
                    if let Ok(delta) = ciborium::from_reader(update.unwrap_delta().as_ref()) {
                        debug!("Successfully deserialized delta");
                        let current_state = room_data.room_state.clone();
                        if let Err(e) = room_data.room_state.apply_delta(
                            &current_state,
                            &room_data.parameters(),
                            &Some(delta)
                        ) {
                            error!("Failed to apply delta: {}", e);
                            *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                            room_data.sync_status = RoomSyncStatus::Error(e);
                        }
                    }
                }
            }
        }
    }

    /// Update room status for a specific owner
    fn update_room_status(&self, owner_key: &VerifyingKey, new_status: RoomSyncStatus) {
        if let Some(rooms) = &self.rooms_signal {
            if let Ok(mut rooms_write) = rooms.try_write() {
                if let Some(room) = rooms_write.map.get_mut(owner_key) {
                    info!("Updating room status for {:?} to {:?}", owner_key, new_status);
                    room.sync_status = new_status;
                }
            }
        }
    }

    /// Process an OK response from the Freenet network
    fn process_ok_response(&self) {
        info!("Received OK response from host");
        *SYNC_STATUS.write() = SyncStatus::Connected;
        
        // Update the status signal if available
        if let Some(status) = &self.status_signal {
            let status = status.clone();
            let _result = status.try_write().map(|mut status_write| {
                *status_write = SyncStatus::Connected;
            });
        }
        
        // Update room statuses if available
        if let Some(rooms) = &self.rooms_signal {
            let rooms = rooms.clone();
            let _result = rooms.try_write().map(|mut rooms_write| {
                for room in rooms_write.map.values_mut() {
                    if matches!(room.sync_status, RoomSyncStatus::Subscribing) {
                        info!("Room subscription confirmed for: {:?}", room.owner_vk);
                        room.sync_status = RoomSyncStatus::Subscribed;
                    } else if matches!(room.sync_status, RoomSyncStatus::Putting) {
                        info!("Room PUT confirmed for: {:?}", room.owner_vk);
                        room.sync_status = RoomSyncStatus::Unsubscribed;
                    }
                }
            });
        }
    }

    /// Set up room subscription and update logic
    fn setup_room_subscriptions(&self, request_sender: UnboundedSender<ClientRequest<'static>>) {
        // Check if we have the rooms signal
        let Some(rooms) = &self.rooms_signal else {
            error!("Cannot set up room subscriptions: Rooms signal not available");
            return;
        };
        
        let request_sender = request_sender.clone();
        let mut rooms_clone = rooms.clone();

        // Track the number of rooms to detect changes
        let mut prev_room_count = 0;

        // Create a shared channel for status updates
        let (status_sender, mut status_receiver) = futures::channel::mpsc::unbounded::<(VerifyingKey, RoomSyncStatus)>();
        
        // Create a clone of self for the async task
        let self_clone = self.clone();
        
        // Spawn a task to process status updates outside of async blocks
        wasm_bindgen_futures::spawn_local(async move {
            while let Some((owner_key, status)) = status_receiver.next().await {
                self_clone.update_room_status(&owner_key, status);
            }
        });

        use_effect(move || {
            // Create a local clone of the rooms to avoid borrowing issues
            let current_room_count = rooms_clone.read().map.len();
            
            // Check if the room count has changed
            if current_room_count != prev_room_count {
                info!("Rooms signal changed: {} -> {} rooms", prev_room_count, current_room_count);
                prev_room_count = current_room_count;
                
                // Process all rooms - get a mutable reference after the read is dropped
                if let Ok(mut rooms_write) = rooms_clone.try_write() {
                    info!("Checking for rooms to synchronize, found {} rooms", rooms_write.map.len());
                    
                    // Clone the status sender for use in async blocks
                    let status_sender = status_sender.clone();
                
                for (owner_vk, room) in rooms_write.map.iter_mut() {
                    // Handle rooms that need to be PUT first
                    if matches!(room.sync_status, RoomSyncStatus::NeedsPut) {
                        info!("Found new room that needs to be PUT with owner: {:?}", owner_vk);
                        info!("Putting room with contract key: {:?}", room.contract_key);
                        room.sync_status = RoomSyncStatus::Putting;
                        
                        // Create the contract container
                        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
                        let parameters = ChatRoomParametersV1 { owner: *owner_vk };
                        let params_bytes = to_cbor_vec(&parameters);
                        // Generate contract key for the room
                        let parameters = Parameters::from(params_bytes.clone());
                        let instance_id = ContractInstanceId::from_params_and_code(parameters.clone(), contract_code.clone());
                        let _contract_key = ContractKey::from(instance_id);
                        
                        // Create the contract container
                        let contract_container = ContractContainer::from(
                            freenet_stdlib::prelude::ContractWasmAPIVersion::V1(
                                freenet_stdlib::prelude::WrappedContract::new(
                                    std::sync::Arc::new(contract_code),
                                    parameters
                                )
                            )
                        );
                        
                        // Prepare the state
                        let state_bytes = to_cbor_vec(&room.room_state);
                        let wrapped_state = WrappedState::new(state_bytes.clone());
                        
                        // Create the PUT request
                        let put_request = ContractRequest::Put {
                            contract: contract_container,
                            state: wrapped_state,
                            related_contracts: RelatedContracts::default(),
                        };
                        
                        let mut sender = request_sender.clone();
                        let _room_key = room.contract_key;
                        let owner_key = *owner_vk;
                        let mut status_sender = status_sender.clone();
                        
                        wasm_bindgen_futures::spawn_local(async move {
                            if let Err(e) = sender.send(put_request.into()).await {
                                error!("Failed to PUT room: {}", e);
                                // Send status update to be processed outside of async block
                                let error_status = RoomSyncStatus::Error(format!("Failed to PUT room: {}", e));
                                if let Err(e) = status_sender.send((owner_key, error_status)).await {
                                    error!("Failed to send status update: {}", e);
                                }
                            } else {
                                info!("Successfully sent PUT request for room");
                            }
                        });
                    }
                    // Subscribe to room if not already subscribed
                    else if matches!(room.sync_status, RoomSyncStatus::Unsubscribed) {
                        info!("Found new unsubscribed room with owner: {:?}", owner_vk);
                        info!("Subscribing to room with contract key: {:?}", room.contract_key);
                        room.sync_status = RoomSyncStatus::Subscribing;
                        let subscribe_request = ContractRequest::Subscribe {
                            key: room.contract_key,
                            summary: None,
                        };
                        let mut sender = request_sender.clone();
                        let owner_key = *owner_vk;
                        let mut status_sender = status_sender.clone();
                        
                        wasm_bindgen_futures::spawn_local(async move {
                            if let Err(e) = sender.send(subscribe_request.into()).await {
                                error!("Failed to subscribe to room: {}", e);
                                // Send status update to be processed outside of async block
                                let error_status = RoomSyncStatus::Error(format!("Failed to subscribe to room: {}", e));
                                if let Err(e) = status_sender.send((owner_key, error_status)).await {
                                    error!("Failed to send status update: {}", e);
                                }
                            } else {
                                info!("Successfully sent subscription request for room");
                            }
                        });
                    }
                    
                    // Always send the current state - clone what we need before the async block
                    let state_bytes = to_cbor_vec(&room.room_state);
                    let contract_key = room.contract_key;
                    let update_request = ContractRequest::Update {
                        key: contract_key,
                        data: freenet_stdlib::prelude::UpdateData::State(
                            state_bytes.clone().into(),
                        ),
                    };
                    info!("Sending room state update for key: {:?}", contract_key);
                    debug!("Update size: {} bytes", state_bytes.len());
                    let mut sender = request_sender.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Err(e) = sender.send(update_request.into()).await {
                            error!("Failed to send room update: {}", e);
                            // Don't change sync status for update failures as they're less critical
                            // and we don't want to overwrite more important status information
                        } else {
                            info!("Successfully sent room state update");
                        }
                    });
                }
            }
        }});
    }

    /// Starts the Freenet API synchronizer
    ///
    /// This initializes the WebSocket connection and starts the coroutine
    /// that handles communication with the Freenet network
    pub fn start(&mut self) {
        info!("FreenetApiSynchronizer::start() called - BEGIN");
        info!("FreenetApiSynchronizer::start() called - using log::info");

        let request_sender = self.sender.request_sender.clone();

        // Set the ready flag in the struct to false initially
        self.ws_ready = false;

        // Create a shared sender that will be used for all requests
        let (shared_sender, _shared_receiver) = futures::channel::mpsc::unbounded();
        
        // Update the sender in our struct
        self.sender.request_sender = shared_sender.clone();

        // Create a clone of self for the coroutine to avoid lifetime issues
        let self_clone = self.clone();

        // Start the sync coroutine
        use_coroutine(move |mut rx| {
            // Clone everything needed for the coroutine
            let request_sender_clone = request_sender.clone();
            
            // Create a channel inside the coroutine closure
            let (internal_sender, mut internal_receiver) = futures::channel::mpsc::unbounded();
            
            // Clone the shared sender for the coroutine
            let _shared_sender_for_coroutine = shared_sender.clone();
            
            // Spawn a task to handle messages from the shared sender
            let internal_sender_clone = internal_sender.clone();
            wasm_bindgen_futures::spawn_local({
                let mut internal_sender = internal_sender_clone;
                async move {
                    // Create a new channel for receiving messages from the shared sender
                    let (_forward_sender, mut forward_receiver) = futures::channel::mpsc::unbounded();
                    
                    // Process messages from the forward receiver
                    while let Some(msg) = forward_receiver.next().await {
                        if let Err(e) = internal_sender.send(msg).await {
                            error!("Failed to forward message to internal channel: {}", e);
                            break;
                        }
                    }
                }
            });
            
            wasm_bindgen_futures::spawn_local({
                let self_clone = self_clone.clone();
                async move {
                // Main connection loop with reconnection logic
                loop {
                    let connection_result = Self::initialize_connection().await;

                    match connection_result {
                        Ok((_websocket_connection, mut web_api)) => {
                            let (_host_response_sender, mut host_response_receiver) =
                                futures::channel::mpsc::unbounded::<Result<freenet_stdlib::client_api::HostResponse, String>>();

                            info!("FreenetApi initialized with WebSocket URL: {}", WEBSOCKET_URL);

                            // Set up room subscriptions and updates
                            self_clone.setup_room_subscriptions(request_sender_clone.clone());

                            // Main event loop
                            loop {
                                futures::select! {
                                    // Handle incoming client requests from the component
                                    msg = rx.next() => {
                                        if let Some(request) = msg {
                                            debug!("Processing client request from component: {:?}", request);
                                            *SYNC_STATUS.write() = SyncStatus::Syncing;
                                            if let Err(e) = web_api.send(request).await {
                                                error!("Failed to send request to WebApi: {}", e);
                                                *SYNC_STATUS.write() = SyncStatus::Error(e.to_string());
                                                break;
                                            } else {
                                                debug!("Successfully sent request to WebApi");
                                            }
                                        }
                                    },

                                    // Handle requests from the internal channel
                                    shared_msg = internal_receiver.next() => {
                                        if let Some(request) = shared_msg {
                                            debug!("Processing client request from shared channel: {:?}", request);
                                            *SYNC_STATUS.write() = SyncStatus::Syncing;
                                            if let Err(e) = web_api.send(request).await {
                                                error!("Failed to send request to WebApi from shared channel: {}", e);
                                                *SYNC_STATUS.write() = SyncStatus::Error(e.to_string());
                                                break;
                                            } else {
                                                debug!("Successfully sent request from shared channel to WebApi");
                                            }
                                        } else {
                                            // Shared receiver closed
                                            error!("Shared receiver channel closed unexpectedly");
                                            break;
                                        }
                                    },

                                    // Handle responses from the host
                                    response = host_response_receiver.next() => {
                                        if let Some(Ok(response)) = response {
                                            match response {
                                                HostResponse::ContractResponse(contract_response) => {
                                                    match contract_response {
                                                        ContractResponse::GetResponse { key, state, .. } => {
                                                            self_clone.process_get_response(key, state.to_vec());
                                                        },
                                                        ContractResponse::UpdateNotification { key, update } => {
                                                            self_clone.process_update_notification(key, update);
                                                        },
                                                        _ => {}
                                                    }
                                                },
                                                HostResponse::Ok => {
                                                    self_clone.process_ok_response();
                                                },
                                                _ => {}
                                            }
                                        } else if let Some(Err(e)) = response {
                                            error!("Error from host response: {}", e);
                                            *SYNC_STATUS.write() = SyncStatus::Error(e.to_string());
                                            break;
                                        } else {
                                            // Host response channel closed
                                            error!("Host response channel closed unexpectedly");
                                            break;
                                        }
                                    }
                                }
                            }

                            // If we get here, the connection was lost
                            error!("WebSocket connection lost or closed, attempting to reconnect in 3 seconds...");
                            *SYNC_STATUS.write() = SyncStatus::Error("Connection lost, attempting to reconnect...".to_string());

                            // Wait before reconnecting
                            sleep(Duration::from_millis(3000)).await;

                            // Break out of the current connection context
                            break;
                        },
                        Err(e) => {
                            // Connection failed, wait before retrying
                            error!("Failed to establish WebSocket connection: {}", e);
                            *SYNC_STATUS.write() = SyncStatus::Error(format!("Connection failed: {}", e));
                            sleep(Duration::from_millis(5000)).await;

                            // Continue to retry
                            continue;
                        }
                    }
                }
            }
        });
            }
        });
    }

    /// Prepares chat room parameters for contract creation
    pub fn prepare_chat_room_parameters(room_owner: &VerifyingKey) -> Parameters {
        let chat_room_params = ChatRoomParametersV1 { owner: *room_owner };
        to_cbor_vec(&chat_room_params).into()
    }

    /// Generates a contract key from parameters and WASM code
    pub fn generate_contract_key(parameters: Parameters) -> ContractKey {
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let instance_id = ContractInstanceId::from_params_and_code(parameters, contract_code);
        ContractKey::from(instance_id)
    }

    /// Subscribes to a chat room owned by the specified room owner
    ///
    /// # Arguments
    /// * `room_owner` - VerifyingKey of the room owner to subscribe to
    ///
    /// # Panics
    /// If unable to send subscription request
    pub async fn subscribe(&mut self, room_owner: &VerifyingKey) {
        info!("Subscribing to chat room owned by {:?}", room_owner);
        let parameters = Self::prepare_chat_room_parameters(room_owner);
        let contract_key = Self::generate_contract_key(parameters);
        let subscribe_request = ContractRequest::Subscribe {
            key: contract_key,
            summary: None,
        };
        self.sender
            .request_sender
            .send(subscribe_request.into())
            .await
            .expect("Unable to send request");
    }

    pub async fn request_room_state(&mut self, room_owner: &VerifyingKey) -> Result<(), String> {
        info!("Requesting room state for room owned by {:?}", room_owner);
        debug!("Current sender state: {:?}", self.sender.request_sender);

        // Check if WebSocket is ready
        let sync_status = match SYNC_STATUS.try_read() {
            Ok(status_ref) => {
                let status = status_ref.clone();
                debug!("Current sync status: {:?}", status);
                if !matches!(status, SyncStatus::Connected | SyncStatus::Syncing) {
                    let error_msg = format!("Cannot request room state: WebSocket not connected (status: {:?})", status);
                    error!("{}", error_msg);
                    return Err(error_msg);
                }
                status
            },
            Err(e) => {
                let error_msg = format!("Cannot request room state: Unable to read sync status: {:?}", e);
                error!("{}", error_msg);
                return Err(error_msg);
            }
        };
        
        debug!("Sync status check passed: {:?}", sync_status);
        let parameters = Self::prepare_chat_room_parameters(room_owner);
        let contract_key = Self::generate_contract_key(parameters);
        let get_request = ContractRequest::Get {
            key: contract_key,
            return_contract_code: false
        };
        debug!("Generated contract key: {:?}", contract_key);

        // Add retry logic for sending the request
        let mut retries = 0;
        const MAX_RETRIES: u8 = 3;

        while retries < MAX_RETRIES {
            debug!("Sending request attempt {}/{}", retries + 1, MAX_RETRIES);
            let mut sender = self.sender.request_sender.clone();
            debug!("Sender cloned, preparing to send request");
            
            match sender.send(get_request.clone().into()).await {
                Ok(_) => {
                    info!("Successfully sent request for room state");
                    return Ok(());
                },
                Err(e) => {
                    let error_msg = format!("Failed to send request (attempt {}/{}): {}",
                                            retries + 1, MAX_RETRIES, e);
                    error!("{}", error_msg);
                    debug!("Detailed error info: {:?}", e);

                    if retries == MAX_RETRIES - 1 {
                        // Last attempt failed, update status and return error
                        *SYNC_STATUS.write() = SyncStatus::Error(error_msg.clone());
                        return Err(error_msg);
                    }

                    // Wait before retrying
                    retries += 1;
                    debug!("Waiting before retry #{}", retries);
                    sleep(Duration::from_millis(500)).await;
                }
            }
        }

        // This should never be reached due to the return in the last retry
        Err("Failed to send request after maximum retries".to_string())
    }
}
