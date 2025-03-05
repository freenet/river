use super::constants::*;
use crate::invites::PendingInvites;
use crate::room_data::{Rooms, RoomData, RoomSyncStatus};
use crate::util::{owner_vk_to_contract_key, sleep, to_cbor_vec};
use dioxus::prelude::*;
use dioxus::logger::tracing::{info, error, debug, warn};
use ed25519_dalek::VerifyingKey;
use std::collections::{HashSet, HashMap};
use std::time::Duration;
use wasm_bindgen_futures::spawn_local;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use freenet_stdlib::{
    client_api::{WebApi, HostResponse},
    prelude::{ContractKey},
};
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse};
use freenet_stdlib::prelude::{ContractCode, ContractInstanceId, Parameters, UpdateData, WrappedState};
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use crate::constants::ROOM_CONTRACT_WASM;

/// Manages synchronization between local room state and Freenet network
pub struct FreenetSynchronizer {
    rooms: Signal<Rooms>,
    websocket: Option<web_sys::WebSocket>,
    web_api: Signal<Option<WebApi>>,
    // Used to show status in UI
    synchronizer_status: Signal<SynchronizerStatus>,
    contract_status: Signal<HashMap<ContractKey, Signal<ContractStatus>>>,
}

enum SynchronizerStatus {
    /// Synchronizer is not connected to Freenet
    Disconnected,
    /// Synchronizer is connecting to Freenet
    Connecting,
    /// Synchronizer is connected to Freenet
    Connected,
    /// Synchronizer encountered an error
    Error(String),
}

/// Tracks operations in progress
enum ContractStatus {
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
        synchronizer_status: Signal<SynchronizerStatus>,
    ) -> Self {
        Self {
            rooms,
            synchronizer_status,
            websocket: None,
            web_api: Signal::new(None),
            contract_status: Signal::new(HashMap::new()),
        }
    }

    pub fn start(&mut self) {
        info!("Starting FreenetSynchronizer");

        use_effect(move || {
            info!("Rooms state changed, checking for sync needs");
            self.process_rooms(self.rooms());

        });

        // Start connection
        self.connect();
    }

    fn connect(&mut self) {
        info!("Connecting to Freenet node at: {}", WEBSOCKET_URL);
        *self.synchronizer_status.write() = SynchronizerStatus::Connecting;
        
        let mut signal_clone = self.clone();

        spawn_local(async move {
            // Initialize connection
            let result = self.initialize_connection().await;
            
            match result {
                Ok(response_rx) => {
                    info!("Successfully connected to Freenet node");
                    {
                        let mut sync = signal_clone.write();
                        sync.is_connected = true;
                    }
                    
                    // Start processing API responses
                    self.process_api_responses(signal_clone.clone(), response_rx);
                    
                    // Process rooms to sync them
                    signal_clone.process_rooms();
                    
                    *self.synchronizer_status.write() = SynchronizerStatus::Connected;
                }
                Err(e) => {
                    error!("Failed to connect to Freenet node: {}", e);
                    *self.synchronizer_status.write() = SynchronizerStatus::Error(e);
                    
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

    fn process_rooms(&mut self, rooms : Rooms) {
        // Iterate through rooms and check which ones need synchronization
        for (room_key, room_data) in rooms.map.iter() {
            match room_data.sync_status {
                RoomSyncStatus::NewlyCreated => {
                    info!("Room needs PUT: {:?}", room_key);
                    // Clone for the async block
                    let room_key_clone = room_key.clone();
                    let mut self_clone = self.clone();
                    
                    // Schedule the PUT operation
                    spawn_local(async move {
                        match self_clone.put_and_subscribe(&room_key_clone).await {
                            Ok(_) => {
                                info!("Successfully PUT room state for {:?}", room_key_clone);
                                todo!("Now subscribe to the room");
                            },
                            Err(e) => error!("Failed to PUT room state: {}", e),
                        }
                    });
                },
                
                
                // We're not subscribing after a put (perhaps after delay), nor changing SyncStatus to what it should
                // be after the operation. 
                
                RoomSyncStatus::Subscribed => {
                    // Check if the room state has changed since last sync
                    if room_data.needs_sync() {
                        info!("Room needs sync: {:?}", room_key);
                        // Check if we're already subscribed
                        if !self.subscribed_contracts.read().contains(&room_data.contract_key) {
                            // Clone for the async block
                            let room_key_clone = room_key.clone();
                            let mut self_clone = self.clone();

                            // Schedule the subscribe operation
                            spawn_local(async move {
                                match self_clone.subscribe(&room_key_clone).await {
                                    Ok(_) => info!("Successfully subscribed to room {:?}", room_key_clone),
                                    Err(e) => error!("Failed to subscribe to room: {}", e),
                                }
                            });
                        }
                    }
                },
                _ => {} // No action needed for other states
            }
        }
        
        drop(rooms); // Explicitly drop the read lock
    }

    async fn put_and_subscribe(&mut self, room_key: &VerifyingKey, state: &ChatRoomStateV1) -> Result<(), String> {
        info!("Putting room state for: {:?}", room_key);
        
        // Get the room data
        let mut rooms = self.rooms.write();
        let room_data = match rooms.map.get_mut(room_key) {
            Some(data) => data,
            None => return Err(format!("Room not found: {:?}", room_key)),
        };
        
        // Serialize the room state using ciberium
        let state_vec = to_cbor_vec(&room_data.room_state);
        
        // Put the contract state
        self.web_api.put_contract_state(&room_data.contract_key, &state_vec)
            .map_err(|e| format!("Failed to put contract state: {}", e))?;

        
        // Update room sync status
        if room_data.sync_status == RoomSyncStatus::NewlyCreated {
            room_data.sync_status = RoomSyncStatus::Putting;
        }

        // Will subscribe when response comes back from PUT

        Ok(())
    }
    
    async fn send(&mut self, request: ClientRequest<'static>) -> Result<(), String> {
        self.web_api.write().or_err("WebAPI not initialized")?.send(request)
    }

    /// Initializes the connection to the Freenet node
    pub async fn initialize_connection(&mut self) -> Result<UnboundedReceiver<Result<HostResponse, String>>, String> {
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
    fn process_api_responses(&mut self, mut response_rx: UnboundedReceiver<Result<HostResponse, String>>) {
        spawn_local(async move {
            info!("Starting API response processor");

            while let Some(response) = response_rx.next().await {
                match response {
                    Ok(api_response) => self.handle_api_response(api_response).await,
                    Err(e) => error!("Error in API response: {}", e),
                }
            }

            warn!("API response stream ended");
        });
    }

    /// Handles individual API responses
    async fn handle_api_response(&mut self, response: HostResponse) {
        match response {
            HostResponse::Ok => {
                info!("Received OK response from API");
            },
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::GetResponse { key, contract, state } => {
                        info!("Received contract state for key: {:?}", key);
                        update_room_state(key, state).await;
                    },
                    ContractResponse::PutResponse { key } => {
                        todo!("Should initiate subscription for this newly created contract");
                    }
                    ContractResponse::UpdateNotification { key, update } => {
                        info!("Received update notification for key: {:?}", key);
                        // Handle update notification
                        match update {
                            UpdateData::State(state) => {

                            }
                            UpdateData::Delta(delta) => {
                                warn!("Received delta update, currently ignored: {:?}", delta);
                            }
                            UpdateData::StateAndDelta { .. } => {
                                warn!("Received state and delta update, currently ignored");
                            }
                            UpdateData::RelatedState { .. } => {
                                warn!("Received related state update, currently ignored");
                            }
                            UpdateData::RelatedDelta { .. } => {
                                warn!("Received related delta update, currently ignored");
                            }
                            UpdateData::RelatedStateAndDelta { .. } => {
                                warn!("Received related state and delta update, currently ignored");
                            }
                        }
                    }
                    ContractResponse::UpdateResponse { key, summary } => {}
                    ContractResponse::SubscribeResponse { key, subscribed } => {}
                    _ => {
                        warn!("Unhandled contract response: {:?}", contract_response);
                    }
                }
            }
            _ => {
                warn!("Unhandled API response: {:?}", response);
            }
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