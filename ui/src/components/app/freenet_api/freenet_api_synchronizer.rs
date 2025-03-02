use super::sync_status::{SyncStatus, SYNC_STATUS};
use super::freenet_api_sender::FreenetApiSender;
use super::constants::WEBSOCKET_URL;
use std::cell::RefCell;
use crate::invites::PendingInvites;
use crate::room_data::{RoomSyncStatus, Rooms};
use crate::components::app::room_state_handler;
use crate::util::{to_cbor_vec, sleep};
use crate::constants::ROOM_CONTRACT_WASM;
use freenet_scaffold::ComposableState;
use dioxus::logger::tracing::{info, error};
use dioxus::prelude::{
    use_coroutine, use_effect, Signal, Writable, Readable,
};
use ed25519_dalek::VerifyingKey;
use futures::{StreamExt, sink::SinkExt, channel::mpsc::UnboundedSender};
use river_common::room_state::ChatRoomStateV1;
use std::collections::HashSet;
use std::time::Duration;
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi},
    prelude::{
        ContractCode, ContractInstanceId, ContractKey, Parameters, ContractContainer,
        WrappedState, RelatedContracts, UpdateData,
    },
};
use futures::future::Either;
use river_common::room_state::ChatRoomParametersV1;
use ciborium::from_reader;
use wasm_bindgen_futures::spawn_local;

// Global sender for API requests
thread_local! {
    static GLOBAL_SENDER: RefCell<Option<UnboundedSender<ClientRequest<'static>>>> = RefCell::new(None);
}

// Helper function to access the global sender from anywhere
fn get_global_sender() -> Option<UnboundedSender<ClientRequest<'static>>> {
    GLOBAL_SENDER.with(|sender| {
        sender.borrow().clone()
    })
}

// Helper function to update the global sender
fn update_global_sender(new_sender: UnboundedSender<ClientRequest<'static>>) {
    GLOBAL_SENDER.with(|sender| {
        *sender.borrow_mut() = Some(new_sender);
        info!("Updated global sender");
    });
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

    pub sync_status: Signal<SyncStatus>,
    
    /// Reference to the rooms signal
    pub rooms: Signal<Rooms>,
    
    /// Reference to pending invites signal
    pub pending_invites: Signal<PendingInvites>,
}

impl FreenetApiSynchronizer {
    /// Creates a new FreenetApiSynchronizer without starting it
    ///
    /// # Returns
    /// New instance of FreenetApiSynchronizer with:
    /// - Empty subscription set
    /// - Request sender initialized
    pub fn new(
        sync_status: Signal<SyncStatus>,
        rooms: Signal<Rooms>,
        pending_invites: Signal<PendingInvites>,
    ) -> Self {
        let subscribed_contracts = HashSet::new();
        let (request_sender, _request_receiver) = futures::channel::mpsc::unbounded();
        let sender_for_struct = request_sender.clone();

        Self {
            subscribed_contracts,
            sender: FreenetApiSender {
                request_sender: sender_for_struct,
            },
            ws_ready: false,
            sync_status,
            rooms,
            pending_invites,
        }
    }

    /// Initialize WebSocket connection to Freenet with a specific response sender
    async fn initialize_connection_with_sender(
        host_response_sender: UnboundedSender<Result<HostResponse, String>>
    ) -> Result<(web_sys::WebSocket, WebApi), String> {
        info!("Starting FreenetApiSynchronizer...");
        info!("Attempting to connect to Freenet node at: {}", WEBSOCKET_URL);
        
        // Update the global status
        *SYNC_STATUS.write() = SyncStatus::Connecting;

        let websocket_connection = match web_sys::WebSocket::new(WEBSOCKET_URL) {
            Ok(ws) => {
                info!("WebSocket created successfully");
                
                // Log WebSocket state
                let ready_state = ws.ready_state();
                info!("WebSocket initial ready state: {}", ready_state);
                
                // 0 = CONNECTING, 1 = OPEN, 2 = CLOSING, 3 = CLOSED
                match ready_state {
                    0 => info!("WebSocket is connecting..."),
                    1 => info!("WebSocket is already open!"),
                    2 => info!("WebSocket is closing (unexpected at this stage)"),
                    3 => info!("WebSocket is closed (unexpected at this stage)"),
                    _ => info!("WebSocket has unknown ready state: {}", ready_state),
                }
                
                ws
            },
            Err(e) => {
                let error_msg = format!("Failed to connect to WebSocket: {:?}", e);
                error!("{}", error_msg);
                error!("This usually means the WebSocket URL is invalid or the Freenet node is not running.");
                error!("Please check that the Freenet node is running and accessible at: {}", WEBSOCKET_URL);
                *SYNC_STATUS.write() = SyncStatus::Error(error_msg.clone());
                return Err(error_msg);
            }
        };

        // Create a shared flag to track connection readiness
        thread_local! {
            static IS_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        }

        let set_ready = || IS_READY.with(|flag| flag.store(true, std::sync::atomic::Ordering::SeqCst));
        let check_ready = || IS_READY.with(|flag| flag.load(std::sync::atomic::Ordering::SeqCst));

        let web_api = WebApi::start(
            websocket_connection.clone(),
            move |result| {
                let mut sender = host_response_sender.clone();
                spawn_local(async move {
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
                set_ready();
            },
        );

        let timeout_promise = async {
            sleep(Duration::from_millis(5000)).await;
            false
        };

        let check_ready = async {
            let mut attempts = 0;
            while attempts < 50 {
                if check_ready() {
                    return true;
                }
                sleep(Duration::from_millis(100)).await;
                attempts += 1;
            }
            false
        };

        let select_result = futures::future::select(
            Box::pin(check_ready),
            Box::pin(timeout_promise)
        ).await;

        match select_result {
            Either::Left((true, _)) => {
                info!("WebSocket connection established successfully");
                Ok((websocket_connection, web_api))
            },
            Either::Left((false, _)) => {
                let error_msg = "WebSocket connection ready check failed".to_string();
                error!("{}", error_msg);
                *SYNC_STATUS.write() = SyncStatus::Error(error_msg.clone());
                Err(error_msg)
            },
            Either::Right((_, _)) => {
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
        state: Vec<u8>,
        rooms: &mut Signal<Rooms>,
        pending_invites: &mut Signal<PendingInvites>,
    ) {
        info!("Received GetResponse for key: {:?}", key);
        info!("Response state size: {} bytes", state.len());

        if let Ok(room_state) = from_reader::<ChatRoomStateV1, &[u8]>(state.as_ref()) {
            info!("Successfully deserialized room state");

            let key_bytes: [u8; 32] = key.id().as_bytes().try_into().expect("Invalid key length");
            // Log the key bytes for debugging
            info!("Attempting to convert key bytes to VerifyingKey: {:?}", key_bytes);
            
            match VerifyingKey::from_bytes(&key_bytes) {
                Ok(room_owner) => {
                    info!("Identified room owner from key: {:?}", room_owner);
                    let mut rooms_write = rooms.write();
                    let mut pending_write = pending_invites.write();

                    info!("Checking if this is a pending invitation");
                    let was_pending = room_state_handler::process_room_state_response(
                        &mut rooms_write,
                        &room_owner,
                        room_state.clone(),
                        key,
                        &mut pending_write,
                    );

                    if was_pending {
                        info!("Processed pending invitation for room owned by: {:?}", room_owner);
                    }

                    if !was_pending {
                        // Regular room state update
                        info!("Processing regular room state update");
                        if let Some(room_data) =
                            rooms_write.map.values_mut().find(|r| r.contract_key == key)
                        {
                            let current_state = room_data.room_state.clone();
                            if let Err(e) = room_data.room_state.merge(
                                &current_state,
                                &room_data.parameters(),
                                &room_state,
                            ) {
                                error!("Failed to merge room state: {}", e);
                                *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
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
                    
                    // Try an alternative approach - look up the room in pending invites by contract key
                    info!("Attempting to find room in pending invites by contract key");
                    let pending_read = pending_invites.read();
                    
                    // First find the matching owner key without mutating anything
                    let matching_owner = pending_read.map.iter()
                        .find_map(|(owner_vk, _)| {
                            let params = ChatRoomParametersV1 { owner: *owner_vk };
                            let params_bytes = to_cbor_vec(&params);
                            let parameters = Parameters::from(params_bytes);
                            let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
                            let instance_id = ContractInstanceId::from_params_and_code(parameters, contract_code);
                            let computed_key = ContractKey::from(instance_id);
                            
                            info!("Checking pending invite for owner: {:?}", owner_vk);
                            info!("Computed key: {:?}, Received key: {:?}", computed_key, key);
                            
                            if computed_key == key {
                                Some(*owner_vk)
                            } else {
                                None
                            }
                        });
                    
                    // Drop the read lock before acquiring write locks
                    drop(pending_read);
                    
                    if let Some(owner_vk) = matching_owner {
                        info!("Found matching pending invitation for key: {:?}", key);
                        let mut rooms_write = rooms.write();
                        let mut pending_write = pending_invites.write();
                        
                        let was_pending = room_state_handler::process_room_state_response(
                            &mut rooms_write,
                            &owner_vk,
                            room_state.clone(),
                            key,
                            &mut pending_write,
                        );
                        
                        if was_pending {
                            info!("Successfully processed pending invitation using alternative method");
                        } else {
                            error!("Failed to process pending invitation using alternative method");
                        }
                    } else {
                        error!("Could not find matching pending invitation for key: {:?}", key);
                    }
                }
            }
        } else {
            error!("Failed to decode room state from bytes: {:?}", state.as_slice());
        }
    }

    /// Process an UpdateNotification from the Freenet network
    fn process_update_notification(key: ContractKey, update: UpdateData, rooms: &mut Signal<Rooms>) {
        info!("Received UpdateNotification for key: {:?}", key);
        let mut rooms = rooms.write();
        let key_bytes: [u8; 32] = key.id().as_bytes().try_into().expect("Invalid key length");
        
        info!("Update notification key bytes: {:?}", key_bytes);
        
        match VerifyingKey::from_bytes(&key_bytes) {
            Ok(verifying_key) => {
                info!("Successfully converted key to VerifyingKey: {:?}", verifying_key);
                
                if let Some(room_data) = rooms.map.get_mut(&verifying_key) {
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
                    error!("No matching room found for update notification with key: {:?}", verifying_key);
                    
                    // Log all room keys for debugging
                    info!("Available room keys:");
                    for (room_key, _) in rooms.map.iter() {
                        info!("  Room key: {:?}", room_key);
                    }
                }
            },
            Err(e) => {
                error!("Failed to convert update notification key to VerifyingKey: {:?}", e);
                
                // Try to find the room by contract key instead
                info!("Attempting to find room by contract key");
                let found = rooms.map.values_mut().find(|r| r.contract_key == key);
                
                if let Some(room_data) = found {
                    info!("Found room by contract key: {:?}", room_data.owner_vk);
                    
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
                                room_data.sync_status = RoomSyncStatus::Error(e);
                            } else {
                                info!("Successfully applied delta update to room state using contract key lookup");
                                // Mark as synced after successful delta application
                                room_data.mark_synced();
                            }
                        },
                        Err(e) => {
                            error!("Failed to deserialize delta: {:?}", e);
                        }
                    }
                } else {
                    error!("Could not find room by contract key either: {:?}", key);
                }
            }
        }
    }

    /// Update room status for a specific owner
    fn update_room_status(owner_key: &VerifyingKey, new_status: RoomSyncStatus, rooms: &mut Signal<Rooms>) {
        if let Ok(mut rooms) = rooms.try_write() {
            if let Some(room) = rooms.map.get_mut(owner_key) {
                info!("Updating room status for {:?} to {:?}", owner_key, new_status);
                room.sync_status = new_status;
            }
        }
    }

    /// Process an OK response from the Freenet network
    fn process_ok_response(sync_status: &mut Signal<SyncStatus>, rooms: &mut Signal<Rooms>) {
        info!("Received OK response from host");
        *SYNC_STATUS.write() = SyncStatus::Connected;
        if let Ok(mut status) = sync_status.try_write() {
            *status = SyncStatus::Connected;
        }
        let mut rooms = rooms.write();
        for room in rooms.map.values_mut() {
            if matches!(room.sync_status, RoomSyncStatus::Subscribing) {
                info!("Room subscription confirmed for: {:?}", room.owner_vk);
                room.sync_status = RoomSyncStatus::Subscribed;
                // Mark as synced after successful subscription
                room.mark_synced();
            } else if matches!(room.sync_status, RoomSyncStatus::Putting) {
                info!("Room PUT confirmed for: {:?}", room.owner_vk);
                room.sync_status = RoomSyncStatus::Unsubscribed;
                // Mark as synced after successful PUT
                room.mark_synced();
                
                // After a successful PUT, we need to send the state update
                let owner_vk = room.owner_vk;
                let contract_key = room.contract_key;
                let state_bytes = crate::util::to_cbor_vec(&room.room_state);
                
                // Clone what we need for the async task
                let request_sender = get_global_sender();
                
                if let Some(mut sender) = request_sender {
                    let update_request = ContractRequest::Update {
                        key: contract_key,
                        data: UpdateData::State(state_bytes.clone().into()),
                    };
                    
                    // Spawn a task to send the update after a delay
                    spawn_local(async move {
                        // Wait longer after PUT to ensure contract is fully registered
                        info!("Delaying state update after successful PUT to allow contract registration (3 seconds)");
                        sleep(Duration::from_secs(3)).await;
                        info!("Delay complete, proceeding with state update for room owned by: {:?}", owner_vk);
                        
                        match sender.send(update_request.into()).await {
                            Ok(_) => {
                                info!("Successfully sent room state update after PUT");
                            },
                            Err(e) => {
                                error!("Failed to send room update after PUT: {}", e);
                            }
                        }
                    });
                } else {
                    error!("Cannot send state update after PUT: no global sender available");
                }
            }
        }
    }

    /// Set up room subscription and update logic
    fn setup_room_subscriptions(
        request_sender: UnboundedSender<ClientRequest<'static>>,
        rooms: Signal<Rooms>,
    ) {
        let (status_sender, mut status_receiver) =
            futures::channel::mpsc::unbounded::<(VerifyingKey, RoomSyncStatus)>();

        // Handle status updates in a separate task
        let mut rooms_clone = rooms.clone();
        spawn_local(async move {
            while let Some((owner_key, status)) = status_receiver.next().await {
                Self::update_room_status(&owner_key, status, &mut rooms_clone);
            }
        });

        // Use coroutine to handle room synchronization
        // This will automatically re-execute when the rooms signal changes
        use_coroutine(move |_rx| {
            let request_sender = request_sender.clone();
            let status_sender = status_sender.clone();
            let rooms = rooms.clone();

            async move {
                loop {
                    // Read the rooms signal - this will cause the coroutine to re-execute
                    // when the rooms signal changes
                    let rooms_snapshot = rooms.read();
                    info!("Checking for rooms to synchronize, found {} rooms", rooms_snapshot.map.len());
                    
                    // Drop the read lock before acquiring write lock
                    drop(rooms_snapshot);
                    
                    // Now get a write lock to process rooms
                    let mut rooms_write = rooms.write();
                    
                    for (owner_vk, room) in rooms_write.map.iter_mut() {
                    // Handle rooms that need to be PUT
                    if matches!(room.sync_status, RoomSyncStatus::NeedsPut) {
                        info!("Found new room that needs to be PUT with owner: {:?}", owner_vk);
                        info!("Putting room with contract key: {:?}", room.contract_key);
                        room.sync_status = RoomSyncStatus::Putting;

                        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
                        let parameters = ChatRoomParametersV1 { owner: *owner_vk };
                        let params_bytes = crate::util::to_cbor_vec(&parameters);
                        let parameters = Parameters::from(params_bytes.clone());
                        let instance_id =
                            ContractInstanceId::from_params_and_code(parameters.clone(), contract_code.clone());
                        let _contract_key = ContractKey::from(instance_id);

                        let contract_container = ContractContainer::from(
                            freenet_stdlib::prelude::ContractWasmAPIVersion::V1(
                                freenet_stdlib::prelude::WrappedContract::new(
                                    std::sync::Arc::new(contract_code),
                                    parameters,
                                ),
                            )
                        );

                        let state_bytes = crate::util::to_cbor_vec(&room.room_state);
                        let wrapped_state = WrappedState::new(state_bytes.clone());

                        let put_request = ContractRequest::Put {
                            contract: contract_container,
                            state: wrapped_state,
                            related_contracts: RelatedContracts::default(),
                        };

                        let mut sender = request_sender_clone.clone();
                        let owner_key = *owner_vk;
                        let mut status_sender = status_sender_clone.clone();
                        let put_request_clone = put_request.clone();
                        let contract_key = room.contract_key;

                        spawn_local(async move {
                            info!("Attempting to PUT room with key: {:?}", contract_key);
                            // Try to get the global sender if available
                            let global_sender = get_global_sender();
                            let result = if let Some(mut global_sender) = global_sender {
                                info!("Using global sender for PUT request");
                                global_sender.send(put_request_clone.into()).await
                            } else {
                                info!("Using local sender for PUT request");
                                sender.send(put_request.into()).await
                            };
                            
                            match result {
                                Ok(_) => {
                                    info!("Successfully sent PUT request for room");
                                },
                                Err(e) => {
                                    error!("Failed to PUT room: {}", e);
                                    let error_status = RoomSyncStatus::Error(format!("Failed to PUT room: {}", e));
                                    if let Err(e) = status_sender.send((owner_key, error_status)).await {
                                        error!("Failed to send status update: {}", e);
                                    }
                                }
                            }
                        });
                    }
                    // Subscribe if Unsubscribed
                    else if matches!(room.sync_status, RoomSyncStatus::Unsubscribed) {
                        info!("Found new unsubscribed room with owner: {:?}", owner_vk);
                        info!("Subscribing to room with contract key: {:?}", room.contract_key);
                        room.sync_status = RoomSyncStatus::Subscribing;
                        let subscribe_request = ContractRequest::Subscribe {
                            key: room.contract_key,
                            summary: None,
                        };

                        let mut sender = request_sender_clone.clone();
                        let owner_key = *owner_vk;
                        let mut status_sender = status_sender_clone.clone();
                        let subscribe_request_clone = subscribe_request.clone();
                        let contract_key = room.contract_key;

                        spawn_local(async move {
                            info!("Attempting to subscribe to room with key: {:?}", contract_key);
                            // Try to get the global sender if available
                            let global_sender = get_global_sender();
                            let result = if let Some(mut global_sender) = global_sender {
                                info!("Using global sender for subscribe request");
                                global_sender.send(subscribe_request_clone.into()).await
                            } else {
                                info!("Using local sender for subscribe request");
                                sender.send(subscribe_request.into()).await
                            };
                            
                            match result {
                                Ok(_) => {
                                    info!("Successfully sent subscription request for room");
                                },
                                Err(e) => {
                                    error!("Failed to subscribe to room: {}", e);
                                    let error_status = RoomSyncStatus::Error(format!("Failed to subscribe to room: {}", e));
                                    if let Err(e) = status_sender.send((owner_key, error_status)).await {
                                        error!("Failed to send status update: {}", e);
                                    }
                                }
                            }
                        });
                    }

                    // Check if room needs synchronization based on state comparison
                    else if room.needs_sync() {
                        info!("Found room that needs state synchronization with owner: {:?}", owner_vk);
                        
                        let state_bytes = crate::util::to_cbor_vec(&room.room_state);
                        let contract_key = room.contract_key;
                        let update_request = ContractRequest::Update {
                            key: contract_key,
                            data: UpdateData::State(state_bytes.clone().into()),
                        };
                        info!("Sending room state update for key: {:?}", contract_key);
                        info!("Update size: {} bytes", state_bytes.len());

                        // Mark the room as synced before sending the update
                        room.mark_synced();
                        
                        let mut sender = request_sender_clone.clone();
                        let update_request_clone = update_request.clone();
                        
                        spawn_local(async move {
                            info!("Attempting to send room state update for key: {:?}", contract_key);
                            // Try to get the global sender if available
                            let global_sender = get_global_sender();
                            let result = if let Some(mut global_sender) = global_sender {
                                info!("Using global sender for update request");
                                global_sender.send(update_request_clone.into()).await
                            } else {
                                info!("Using local sender for update request");
                                sender.send(update_request.into()).await
                            };
                            
                            match result {
                                Ok(_) => {
                                    info!("Successfully sent room state update");
                                },
                                Err(e) => {
                                    error!("Failed to send room update: {}", e);
                                }
                            }
                        });
                    }
                    
                    // Only send the current state if we're not in the process of putting the contract
                    // and we're not already handling a needs_sync check
                    if !matches!(room.sync_status, RoomSyncStatus::Putting) && !room.needs_sync() {
                        let state_bytes = crate::util::to_cbor_vec(&room.room_state);
                        let contract_key = room.contract_key;
                        let update_request = ContractRequest::Update {
                            key: contract_key,
                            data: UpdateData::State(state_bytes.clone().into()),
                        };
                        info!("Sending room state update for key: {:?}", contract_key);
                        info!("Update size: {} bytes", state_bytes.len());

                        let mut sender = request_sender_clone.clone();
                        let update_request_clone = update_request.clone();
                        
                        spawn_local(async move {
                            info!("Attempting to send room state update for key: {:?}", contract_key);
                            // Try to get the global sender if available
                            let global_sender = get_global_sender();
                            let result = if let Some(mut global_sender) = global_sender {
                                info!("Using global sender for update request");
                                global_sender.send(update_request_clone.into()).await
                            } else {
                                info!("Using local sender for update request");
                                sender.send(update_request.into()).await
                            };
                            
                            match result {
                                Ok(_) => {
                                    info!("Successfully sent room state update");
                                },
                                Err(e) => {
                                    error!("Failed to send room update: {}", e);
                                }
                            }
                        });
                    }
                    }
                    
                    // Drop the write lock
                    drop(rooms_write);
                    
                    // Wait for a short time before checking again
                    // This is just a safety measure - the coroutine should only re-execute
                    // when the rooms signal changes
                    sleep(Duration::from_secs(5)).await;
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

        self.ws_ready = false;

        // Clone all the fields we need from self to avoid borrowing issues
        let mut sync_status_signal = self.sync_status.clone();
        let mut rooms_signal = self.rooms.clone();
        let pending_invites_signal = self.pending_invites.clone();
        
        // We'll use the module-level global sender functions
        
        // Create a sender to update the FreenetApiSender
        let (sender_update_tx, mut sender_update_rx) = futures::channel::mpsc::unbounded::<UnboundedSender<ClientRequest<'static>>>();
        
        // Clone the sender_update_tx for use in the coroutine
        let sender_update_tx_for_coroutine = sender_update_tx.clone();
        
        // Spawn a task to update the sender
        spawn_local({
            let mut sender = self.sender.clone();
            async move {
                info!("Starting sender update task");
                let result = async {
                    while let Some(new_sender) = sender_update_rx.next().await {
                        info!("Received new sender to update");
                        sender.request_sender = new_sender.clone();
                        update_global_sender(new_sender);
                        info!("Sender updated successfully");
                    }
                    info!("Sender update receiver stream ended normally");
                    Ok::<_, String>(())
                }.await;
                
                if let Err(e) = result {
                    error!("Sender update task error: {}", e);
                }
                error!("Sender update task ended unexpectedly - this may indicate that sender_update_tx was dropped");
                
                // Try to diagnose why the task ended
                if sender_update_tx.is_closed() {
                    error!("Sender update channel is closed");
                } else {
                    error!("Sender update channel appears open, but receiver ended");
                }
            }
        });

        use_coroutine(move |mut rx: futures::channel::mpsc::UnboundedReceiver<ClientRequest<'static>>| {
            // Create all channels inside the coroutine to avoid ownership issues
            let (shared_sender, mut shared_receiver) = futures::channel::mpsc::unbounded();
            
            // Send the new sender to update the FreenetApiSender
            let mut sender_update_tx_clone = sender_update_tx_for_coroutine.clone();
            let shared_sender_clone = shared_sender.clone();
            spawn_local(async move {
                info!("Attempting to update sender");
                if sender_update_tx_clone.is_closed() {
                    error!("Cannot update sender: channel is already closed before sending");
                    return;
                }
                
                match sender_update_tx_clone.send(shared_sender_clone.clone()).await {
                    Ok(_) => info!("Successfully sent sender update"),
                    Err(e) => error!("Failed to update sender: {}", e),
                }
                
                // Keep the sender_update_tx_clone alive for a while to prevent it from being dropped
                sleep(Duration::from_secs(60)).await;
                info!("Sender update task completed after keeping channel alive");
            });
            
            // Add a delay to ensure the sender is updated before we start using it
            spawn_local(async move {
                sleep(Duration::from_millis(100)).await;
                info!("Sender update delay completed");
            });
            
            let request_sender_clone = request_sender.clone();
            let (internal_sender, mut internal_receiver) = futures::channel::mpsc::unbounded();
            // Create a channel to forward messages from the shared sender to the internal receiver
            let internal_sender_clone = internal_sender.clone();
            
            // Start a task to forward messages from shared_receiver to internal_sender
            spawn_local({
                let mut internal_sender = internal_sender_clone.clone();
                async move {
                    info!("Starting shared receiver forwarding task");
                    let result = async {
                        while let Some(msg) = shared_receiver.next().await {
                            info!("Forwarding message from shared channel to internal channel");
                            if let Err(e) = internal_sender.send(msg).await {
                                return Err(format!("Failed to forward message: {}", e));
                            }
                        }
                        Ok(())
                    }.await;
                    
                    match result {
                        Ok(_) => info!("Shared receiver stream ended normally"),
                        Err(e) => error!("Shared receiver forwarding task error: {}", e),
                    }
                    
                    error!("Shared receiver forwarding task ended - this may indicate that shared_sender was dropped");
                    
                    // Try to diagnose why the task ended
                    if internal_sender.is_closed() {
                        error!("Internal sender channel is closed");
                    } else {
                        error!("Internal sender channel appears open, but shared_receiver ended");
                    }
                }
            });

            async move {
                loop {
                    // Create a channel for host responses that will be used throughout the connection
                    let (host_response_sender, mut host_response_receiver) =
                        futures::channel::mpsc::unbounded::<Result<HostResponse, String>>();
                    
                    let connection_result = {
                        // Clone the sender for the WebApi callback
                        let host_response_sender_clone = host_response_sender.clone();
                        
                        // Initialize connection with the cloned sender
                        Self::initialize_connection_with_sender(host_response_sender_clone).await
                    };

                    match connection_result {
                        Ok((_websocket_connection, mut web_api)) => {
                            info!("FreenetApi initialized with WebSocket URL: {}", WEBSOCKET_URL);

                            // Use the cloned rooms signal
                            let rooms_for_subscription = rooms_signal.clone();
                            Self::setup_room_subscriptions(request_sender_clone.clone(), rooms_for_subscription);

                            loop {
                                futures::select! {
                                    msg = rx.next() => {
                                        if let Some(request) = msg {
                                            info!("Processing client request from component: {:?}", request);
                                            *SYNC_STATUS.write() = SyncStatus::Syncing;
                                            match web_api.send(request.clone()).await {
                                                Ok(_) => {
                                                    info!("Successfully sent request to WebApi");
                                                },
                                                Err(e) => {
                                                    error!("Failed to send request to WebApi: {}", e);
                                                    *SYNC_STATUS.write() = SyncStatus::Error(e.to_string());
                                                    
                                                    // Update room status if this was a PUT request
                                                    if let ClientRequest::ContractOp(ContractRequest::Put { .. }) = &request {
                                                        info!("PUT request failed, updating room status");
                                                        let mut rooms_write = rooms_signal.write();
                                                        for room in rooms_write.map.values_mut() {
                                                            if matches!(room.sync_status, RoomSyncStatus::Putting) {
                                                                room.sync_status = RoomSyncStatus::Error(format!("Failed to PUT room: {}", e));
                                                            }
                                                        }
                                                    }
                                                    
                                                    // Don't break here, just log the error and continue
                                                }
                                            }
                                        }
                                    },

                                    shared_msg = internal_receiver.next() => {
                                        if let Some(request) = shared_msg {
                                            info!("Processing client request from shared channel: {:?}", request);
                                            *SYNC_STATUS.write() = SyncStatus::Syncing;
                                            info!("Sending request to WebApi from shared channel");
                                            match web_api.send(request.clone()).await {
                                                Ok(_) => {
                                                    info!("Successfully sent request from shared channel to WebApi");
                                                },
                                                Err(e) => {
                                                    error!("Failed to send request to WebApi from shared channel: {}", e);
                                                    *SYNC_STATUS.write() = SyncStatus::Error(e.to_string());
                                                        
                                                    // Update room status if this was a PUT request
                                                    if let ClientRequest::ContractOp(ContractRequest::Put { .. }) = &request {
                                                        info!("PUT request failed, updating room status");
                                                        let mut rooms_write = rooms_signal.write();
                                                        for room in rooms_write.map.values_mut() {
                                                            if matches!(room.sync_status, RoomSyncStatus::Putting) {
                                                                room.sync_status = RoomSyncStatus::Error(format!("Failed to PUT room: {}", e));
                                                            }
                                                        }
                                                    }
                                                        
                                                    // Don't break here, just log the error and continue
                                                }
                                            }
                                        } else {
                                            error!("Shared receiver channel closed unexpectedly");
                                            // Don't break here, just log the error and continue
                                        }
                                    },

                                    response = host_response_receiver.next() => {
                                        if let Some(Ok(response)) = response {
                                            match response {
                                                HostResponse::ContractResponse(contract_response) => {
                                                    match contract_response {
                                                        ContractResponse::GetResponse { key, state, .. } => {
                                                            // Use the cloned signals
                                                            let mut rooms_clone = rooms_signal.clone();
                                                            let mut pending_invites_clone = pending_invites_signal.clone();
                                                            Self::process_get_response(key, state.to_vec(), &mut rooms_clone, &mut pending_invites_clone);
                                                        },
                                                        ContractResponse::UpdateNotification { key, update } => {
                                                            let mut rooms_clone = rooms_signal.clone();
                                                            Self::process_update_notification(key, update, &mut rooms_clone);
                                                        },
                                                        ContractResponse::PutResponse { key } => {
                                                            info!("Received PutResponse for key: {:?}", key);
                                                            // Find the room with this contract key and update its status
                                                            let mut rooms_write = rooms_signal.write();
                                                            for room in rooms_write.map.values_mut() {
                                                                if room.contract_key == key {
                                                                    info!("Room PUT confirmed for: {:?}", room.owner_vk);
                                                                    if matches!(room.sync_status, RoomSyncStatus::Putting) {
                                                                        room.sync_status = RoomSyncStatus::Unsubscribed;
                                                                        
                                                                        // After a successful PUT, we need to send the state update
                                                                        let owner_vk = room.owner_vk;
                                                                        let contract_key = room.contract_key;
                                                                        let state_bytes = crate::util::to_cbor_vec(&room.room_state);
                                                                        
                                                                        // Clone what we need for the async task
                                                                        let request_sender = get_global_sender();
                                                                        
                                                                        if let Some(mut sender) = request_sender {
                                                                            let update_request = ContractRequest::Update {
                                                                                key: contract_key,
                                                                                data: UpdateData::State(state_bytes.clone().into()),
                                                                            };
                                                                            
                                                                            // Spawn a task to send the update after a delay
                                                                            spawn_local(async move {
                                                                                // Wait longer after PUT to ensure contract is fully registered
                                                                                info!("Delaying state update after successful PUT to allow contract registration (3 seconds)");
                                                                                sleep(Duration::from_secs(3)).await;
                                                                                info!("Delay complete, proceeding with state update for room owned by: {:?}", owner_vk);
                                                                                
                                                                                match sender.send(update_request.into()).await {
                                                                                    Ok(_) => {
                                                                                        info!("Successfully sent room state update after PUT");
                                                                                    },
                                                                                    Err(e) => {
                                                                                        error!("Failed to send room update after PUT: {}", e);
                                                                                    }
                                                                                }
                                                                            });
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        },
                                                        _ => {}
                                                    }
                                                },
                                                HostResponse::Ok => {
                                                    let mut sync_status_clone = sync_status_signal.clone();
                                                    let mut rooms_clone = rooms_signal.clone();
                                                    Self::process_ok_response(&mut sync_status_clone, &mut rooms_clone);
                                                },
                                                _ => {}
                                            }
                                        } else if let Some(Err(e)) = response {
                                            error!("Error from host response: {}", e);
                                            
                                            // Add more detailed logging for node not available errors
                                            if e.contains("node not available") {
                                                error!("Node not available error detected. This usually means the Freenet node is not running or is unreachable.");
                                                error!("Please check that the Freenet node is running at {}", WEBSOCKET_URL);
                                                error!("Will attempt to reconnect in 3 seconds...");
                                            }
                                            
                                            *SYNC_STATUS.write() = SyncStatus::Error(e.to_string());
                                            break;
                                        } else {
                                            error!("Host response channel closed unexpectedly");
                                            break;
                                        }
                                    }
                                }
                            }

                            error!("WebSocket connection lost or closed, attempting to reconnect in 3 seconds...");
                            
                            // Log more details about the connection state
                            info!("Connection details before reconnect attempt:");
                            info!("WebSocket readyState: {}", _websocket_connection.ready_state());
                            info!("Global sender is available: {}", get_global_sender().is_some());
                            
                            *SYNC_STATUS.write() = SyncStatus::Error("Connection lost, attempting to reconnect...".to_string());
                            if let Ok(mut status) = sync_status_signal.try_write() {
                                *status = SyncStatus::Error("Connection lost, attempting to reconnect...".to_string());
                            }
                            
                            // Log reconnection attempt
                            info!("Waiting 3 seconds before reconnection attempt...");
                            sleep(Duration::from_millis(3000)).await;
                            info!("Attempting to reconnect to WebSocket...");
                            continue; // Try to reconnect instead of breaking out
                        },
                        Err(e) => {
                            error!("Failed to establish WebSocket connection: {}", e);
                            
                            // Add more detailed error information
                            if e.contains("WebSocket connection timed out") {
                                error!("WebSocket connection timed out. Please check that the Freenet node is running at {}", WEBSOCKET_URL);
                                error!("If the node is running, check for network issues or firewall settings that might block the connection.");
                            } else if e.contains("Failed to connect to WebSocket") {
                                error!("Failed to connect to WebSocket. This usually means the WebSocket URL is incorrect or the node is not running.");
                                error!("Current WebSocket URL: {}", WEBSOCKET_URL);
                            }
                            
                            *SYNC_STATUS.write() = SyncStatus::Error(format!("Connection failed: {}", e));
                            if let Ok(mut status) = sync_status_signal.try_write() {
                                *status = SyncStatus::Error(format!("Connection failed: {}", e));
                            }
                            
                            info!("Waiting 5 seconds before reconnection attempt...");
                            sleep(Duration::from_millis(5000)).await;
                            info!("Attempting to reconnect to WebSocket...");
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

    /// Requests room state for a specific room
    pub async fn request_room_state(&mut self, room_owner: &VerifyingKey) -> Result<(), String> {
        info!("Requesting room state for room owned by {:?}", room_owner);
        info!("Current sender state: {:?}", self.sender.request_sender);

        // Add more detailed debugging about the sender channel
        let is_closed = self.sender.request_sender.is_closed();
        info!("Sender channel is_closed: {}", is_closed);
        
        // Check if global sender is available
        let global_sender = get_global_sender();
        info!("Global sender available: {}", global_sender.is_some());
        
        // If local sender is closed but global sender is available, we can still proceed
        if is_closed && global_sender.is_none() {
            error!("Cannot request room state: Sender channel is closed and no global sender available");
            return Err("Sender channel is closed".to_string());
        }

        let sync_status = match SYNC_STATUS.try_read() {
            Ok(status_ref) => {
                let status = status_ref.clone();
                info!("Current sync status: {:?}", status);
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

        info!("Sync status check passed: {:?}", sync_status);
        let parameters = Self::prepare_chat_room_parameters(room_owner);
        let contract_key = Self::generate_contract_key(parameters);
        let get_request = ContractRequest::Get {
            key: contract_key,
            return_contract_code: false
        };
        info!("Generated contract key: {:?}", contract_key);

        let mut retries = 0;
        const MAX_RETRIES: u8 = 3;

        while retries < MAX_RETRIES {
            info!("Sending request attempt {}/{}", retries + 1, MAX_RETRIES);
            
            // Try to use global sender first, fall back to local sender
            let global_sender = get_global_sender();
            let send_result = if let Some(mut global_sender) = global_sender {
                info!("Using global sender for room state request");
                global_sender.send(get_request.clone().into()).await
            } else {
                info!("Using local sender for room state request");
                let mut sender = self.sender.request_sender.clone();
                sender.send(get_request.clone().into()).await
            };
            
            match send_result {
                Ok(_) => {
                    info!("Successfully sent request for room state");
                    return Ok(());
                },
                Err(e) => {
                    let error_msg = format!("Failed to send request (attempt {}/{}): {}", retries + 1, MAX_RETRIES, e);
                    error!("{}", error_msg);
                    info!("Detailed error info: {:?}", e);

                    if retries == MAX_RETRIES - 1 {
                        *SYNC_STATUS.write() = SyncStatus::Error(error_msg.clone());
                        return Err(error_msg);
                    }

                    retries += 1;
                    info!("Waiting before retry #{}", retries);
                    sleep(Duration::from_millis(500)).await;
                }
            }
        }

        Err("Failed to send request after maximum retries".to_string())
    }
}
