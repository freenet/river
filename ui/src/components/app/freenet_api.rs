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
    prelude::{ContractCode, ContractInstanceId, ContractKey, Parameters},
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
static SYNC_STATUS: GlobalSignal<SyncStatus> = Global::new(|| SyncStatus::Connecting);

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
        }
    }

    /// Initialize WebSocket connection to Freenet
    async fn initialize_connection() -> Result<(web_sys::WebSocket, WebApi), String> {
        info!("Starting FreenetApiSynchronizer...");
        *SYNC_STATUS.write() = SyncStatus::Connecting;

        let websocket_connection = match web_sys::WebSocket::new(WEBSOCKET_URL) {
            Ok(ws) => {
                info!("WebSocket created successfully");
                ws
            },
            Err(e) => {
                let error_msg = format!("Failed to connect to WebSocket: {:?}", e);
                error!("{}", error_msg);
                *SYNC_STATUS.write() = SyncStatus::Error(error_msg.clone());
                return Err(error_msg);
            }
        };

        let (host_response_sender, _host_response_receiver) =
            futures::channel::mpsc::unbounded::<Result<freenet_stdlib::client_api::HostResponse, String>>();

        // Create a shared flag to track connection readiness
        // Use a static AtomicBool to avoid lifetime issues with async blocks
        static IS_READY: once_cell::sync::Lazy<std::sync::atomic::AtomicBool> = 
            once_cell::sync::Lazy::new(|| std::sync::atomic::AtomicBool::new(false));
        
        // Reset the flag to false for this connection attempt
        IS_READY.store(false, std::sync::atomic::Ordering::SeqCst);

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
                // Signal that the connection is ready
                IS_READY.store(true, std::sync::atomic::Ordering::SeqCst);
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
                if IS_READY.load(std::sync::atomic::Ordering::SeqCst) {
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
        key: ContractKey, 
        state: Vec<u8>
    ) {
        info!("Received GetResponse for key: {:?}", key);
        debug!("Response state size: {} bytes", state.len());

        // Get context outside of conditional blocks
        let mut rooms = use_context::<Signal<Rooms>>();
        let mut pending_invites = use_context::<Signal<PendingInvites>>();
        
        // Update rooms with received state
        if let Ok(room_state) = ciborium::from_reader::<ChatRoomStateV1, &[u8]>(state.as_ref()) {
            debug!("Successfully deserialized room state");

            // Try to find the room owner from the key
            let key_bytes: [u8; 32] = key.id().as_bytes().try_into().expect("Invalid key length");
            if let Ok(room_owner) = VerifyingKey::from_bytes(&key_bytes) {
                info!("Identified room owner from key: {:?}", room_owner);
                let mut rooms_write = rooms.write();
                let mut pending_write = pending_invites.write();

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
            } else {
                error!("Failed to convert key to VerifyingKey");
            }
        } else {
            error!("Failed to decode room state from bytes: {:?}", state.as_slice());
        }
    }

    /// Process an UpdateNotification from the Freenet network
    fn process_update_notification(key: ContractKey, update: freenet_stdlib::prelude::UpdateData) {
        info!("Received UpdateNotification for key: {:?}", key);
        // Handle incremental updates
        let mut rooms = use_context::<Signal<Rooms>>();
        let mut rooms = rooms.write();
        let key_bytes: [u8; 32] = key.id().as_bytes().try_into().expect("Invalid key length");
        if let Some(room_data) = rooms.map.get_mut(&VerifyingKey::from_bytes(&key_bytes).expect("Invalid key bytes")) {
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

    /// Process an OK response from the Freenet network
    fn process_ok_response() {
        info!("Received OK response from host");
        *SYNC_STATUS.write() = SyncStatus::Connected;
        // Update room status to Subscribed when subscription succeeds
        let mut rooms = use_context::<Signal<Rooms>>();
        let mut rooms = rooms.write();
        for room in rooms.map.values_mut() {
            if matches!(room.sync_status, RoomSyncStatus::Subscribing) {
                info!("Room subscription confirmed for: {:?}", room.owner_vk);
                room.sync_status = RoomSyncStatus::Subscribed;
            }
        }
    }

    /// Set up room subscription and update logic
    fn setup_room_subscriptions(request_sender: UnboundedSender<ClientRequest<'static>>) {
        // Watch for changes to Rooms signal
        let mut rooms = use_context::<Signal<Rooms>>();
        let request_sender = request_sender.clone();

        use_effect(move || {
            {
                let mut rooms = rooms.write();
                for room in rooms.map.values_mut() {
                    // Subscribe to room if not already subscribed
                    if matches!(room.sync_status, RoomSyncStatus::Unsubscribed) {
                        info!("Subscribing to room with contract key: {:?}", room.contract_key);
                        room.sync_status = RoomSyncStatus::Subscribing;
                        let subscribe_request = ContractRequest::Subscribe {
                            key: room.contract_key,
                            summary: None,
                        };
                        let mut sender = request_sender.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            if let Err(e) = sender.send(subscribe_request.into()).await {
                                error!("Failed to subscribe to room: {}", e);
                            } else {
                                debug!("Successfully sent subscription request");
                            }
                        });
                    }
                    let state_bytes = to_cbor_vec(&room.room_state);
                    let update_request = ContractRequest::Update {
                        key: room.contract_key,
                        data: freenet_stdlib::prelude::UpdateData::State(
                            state_bytes.clone().into(),
                        ),
                    };
                    info!("Sending room state update for key: {:?}", room.contract_key);
                    debug!("Update size: {} bytes", state_bytes.len());
                    let mut sender = request_sender.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Err(e) = sender.send(update_request.into()).await {
                            error!("Failed to send room update: {}", e);
                        } else {
                            debug!("Successfully sent room state update");
                        }
                    });
                }
            }
        });
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
                            Self::setup_room_subscriptions(request_sender_clone.clone());

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
                                                            Self::process_get_response(key, state.to_vec());
                                                        },
                                                        ContractResponse::UpdateNotification { key, update } => {
                                                            Self::process_update_notification(key, update);
                                                        },
                                                        _ => {}
                                                    }
                                                },
                                                HostResponse::Ok => {
                                                    Self::process_ok_response();
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

    /// Prepares chat room parameters for contract creation
    fn prepare_chat_room_parameters(room_owner: &VerifyingKey) -> Parameters {
        let chat_room_params = ChatRoomParametersV1 { owner: *room_owner };
        to_cbor_vec(&chat_room_params).into()
    }

    /// Generates a contract key from parameters and WASM code
    fn generate_contract_key(parameters: Parameters) -> ContractKey {
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
