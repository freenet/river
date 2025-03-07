use super::connection_manager::ConnectionManager;
use super::error::SynchronizerError;
use super::response_handler::ResponseHandler;
use super::room_synchronizer::RoomSynchronizer;
use crate::room_data::Rooms;
use dioxus::prelude::*;
use dioxus::logger::tracing::{info, error, warn};
use wasm_bindgen_futures::spawn_local;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use freenet_stdlib::client_api::HostResponse;

/// Message types for communicating with the synchronizer
pub enum SynchronizerMessage {
    ProcessRooms,
    Connect,
    ApiResponse(Result<HostResponse, SynchronizerError>),
}

/// Manages synchronization between local room state and Freenet network
pub struct FreenetSynchronizer {
    // Channel for sending messages to the synchronizer
    pub message_tx: UnboundedSender<SynchronizerMessage>,
    message_rx: Option<UnboundedReceiver<SynchronizerMessage>>,
    // Components
    connection_manager: ConnectionManager,
    response_handler: ResponseHandler,
}

pub enum SynchronizerStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

// Add conversion from SynchronizerError to SynchronizerStatus
impl From<SynchronizerError> for SynchronizerStatus {
    fn from(error: SynchronizerError) -> Self {
        SynchronizerStatus::Error(error.to_string())
    }
}

impl FreenetSynchronizer {
    pub fn new(
        rooms: Signal<Rooms>,
        synchronizer_status: Signal<SynchronizerStatus>,
    ) -> Self {
        let (message_tx, message_rx) = unbounded();
        
        let room_synchronizer = RoomSynchronizer::new(rooms.clone());
        let response_handler = ResponseHandler::new(room_synchronizer);
        let connection_manager = ConnectionManager::new(synchronizer_status);
        
        Self {
            message_tx,
            message_rx: Some(message_rx),
            connection_manager,
            response_handler,
        }
    }

    pub async fn start(&mut self) {
        info!("Starting FreenetSynchronizer");
        
        // Take ownership of the receiver
        let mut message_rx = self.message_rx.take().expect("Message receiver already taken");
        let message_tx = self.message_tx.clone();
        
        // Set up effect to monitor room changes
        let effect_tx = self.message_tx.clone();
        use_effect(move || {
            info!("Rooms state changed, checking for sync needs");
            // Send a message to process rooms instead of calling directly
            spawn_local({
                let tx = effect_tx.clone();
                async move {
                    if let Err(e) = tx.unbounded_send(SynchronizerMessage::ProcessRooms) {
                        error!("Failed to send ProcessRooms message: {}", e);
                    }
                }
            });
        });

        // Start the message processing loop in a separate task
        // Move the components out of self to use in the async task
        let mut connection_manager = ConnectionManager::new(self.connection_manager.get_status_signal().clone());
        std::mem::swap(&mut connection_manager, &mut self.connection_manager);
        
        let mut response_handler = ResponseHandler::new(self.response_handler.take_room_synchronizer());
        
        spawn_local(async move {
            // Start connection
            if let Err(e) = message_tx.unbounded_send(SynchronizerMessage::Connect) {
                error!("Failed to send Connect message: {}", e);
            }
            
            // Process messages
            while let Some(msg) = message_rx.next().await {
                match msg {
                    SynchronizerMessage::ProcessRooms => {
                        if let Some(web_api) = connection_manager.get_api_mut() {
                            if let Err(e) = response_handler.get_room_synchronizer_mut().process_rooms(web_api).await {
                                error!("Error processing rooms: {}", e);
                            }
                        } else {
                            error!("Cannot process rooms: API not initialized");
                        }
                    },
                    SynchronizerMessage::Connect => {
                        match connection_manager.initialize_connection(message_tx.clone()).await {
                            Ok(()) => {
                                info!("Connection established successfully");
                                
                                // Process rooms to sync them
                                if let Some(web_api) = connection_manager.get_api_mut() {
                                    if let Err(e) = response_handler.get_room_synchronizer_mut().process_rooms(web_api).await {
                                        error!("Error processing rooms after connection: {}", e);
                                    }
                                }
                            },
                            Err(e) => {
                                error!("Failed to initialize connection: {}", e);
                                // Reconnection is handled by the connection manager
                            }
                        }
                    },
                    SynchronizerMessage::ApiResponse(response) => {
                        match response {
                            Ok(api_response) => {
                                if let Some(web_api) = connection_manager.get_api_mut() {
                                    if let Err(e) = response_handler.handle_api_response(api_response, web_api).await {
                                        error!("Error handling API response: {}", e);
                                    }
                                } else {
                                    error!("Cannot handle API response: API not initialized");
                                }
                            },
                            Err(e) => error!("Error in API response: {}", e),
                        }
                    }
                }
            }
            
            warn!("Synchronizer message loop ended");
        });
    }

    // Helper method to send a connect message
    pub fn connect(&self) {
        if let Err(e) = self.message_tx.unbounded_send(SynchronizerMessage::Connect) {
            error!("Failed to send Connect message: {}", e);
        }
    }
}
/*



/// Updates room state from contract state
async fn update_room_state(mut synchronizer: Signal<FreenetSynchronizer>, contract: ContractKey, state: WrappedState) {
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
                    
                    match sync_clone.subscribe(&room_key_clone).await {
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

*/
