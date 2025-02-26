//! Core API functionality for Freenet integration

use crate::components::app::freenet::connection;
use crate::components::app::freenet::processor;
use crate::components::app::freenet::subscription;
use crate::components::app::freenet::types::{FreenetApiSender, SYNC_STATUS, SyncStatus};
use dioxus::prelude::use_coroutine;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse};
use freenet_stdlib::prelude::ContractKey;
use futures::{SinkExt, StreamExt};
use log::{debug, error, info};
use std::collections::HashSet;

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

    /// Starts the Freenet API synchronizer
    ///
    /// This initializes the WebSocket connection and starts the coroutine
    /// that handles communication with the Freenet network
    pub fn start(&mut self) {
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
                    let (forward_sender, mut forward_receiver) = futures::channel::mpsc::unbounded();
                    
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
                    let connection_result = connection::initialize_connection().await;

                    match connection_result {
                        Ok((_websocket_connection, mut web_api)) => {
                            let (_host_response_sender, mut host_response_receiver) =
                                futures::channel::mpsc::unbounded::<Result<freenet_stdlib::client_api::HostResponse, String>>();

                            info!("FreenetApi initialized with WebSocket URL");

                            // Set up room subscriptions and updates
                            subscription::setup_room_subscriptions(request_sender_clone.clone());

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
                                                            processor::process_get_response(key, state.to_vec());
                                                        },
                                                        ContractResponse::UpdateNotification { key, update } => {
                                                            processor::process_update_notification(key, update);
                                                        },
                                                        _ => {}
                                                    }
                                                },
                                                HostResponse::Ok => {
                                                    processor::process_ok_response();
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
                            let _ = futures_timer::Delay::new(std::time::Duration::from_secs(3)).await;

                            // Break out of the current connection context
                            break;
                        },
                        Err(e) => {
                            // Connection failed, wait before retrying
                            error!("Failed to establish WebSocket connection: {}", e);
                            *SYNC_STATUS.write() = SyncStatus::Error(format!("Connection failed: {}", e));
                            let _ = futures_timer::Delay::new(std::time::Duration::from_secs(5)).await;

                            // Continue to retry
                            continue;
                        }
                    }
                }
            }
        });
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
        let parameters = subscription::prepare_chat_room_parameters(room_owner);
        let contract_key = subscription::generate_contract_key(parameters);
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
        subscription::request_room_state(&mut self.sender.request_sender, room_owner).await
    }
}
//! Core API functionality for Freenet integration

use std::collections::HashSet;
use crate::components::app::freenet::connection;
use crate::components::app::freenet::processor;
use crate::components::app::freenet::subscription;
use crate::components::app::freenet::types::{FreenetApiSender, SYNC_STATUS, SyncStatus, WEBSOCKET_URL};
use dioxus::prelude::{use_coroutine, use_context, use_effect, Signal, Writable};
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse, WebApi};
use freenet_stdlib::prelude::{ContractKey, ContractCode, ContractInstanceId, Parameters};
use futures::{SinkExt, StreamExt};
use log::{debug, error, info};
use crate::constants::ROOM_CONTRACT_WASM;
use crate::invites::PendingInvites;
use crate::room_data::{Rooms, RoomSyncStatus};
use crate::util::to_cbor_vec;
use river_common::room_state::ChatRoomParametersV1;

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

    /// Starts the Freenet API synchronizer
    ///
    /// This initializes the WebSocket connection and starts the coroutine
    /// that handles communication with the Freenet network
    pub fn start(&mut self) {
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
                    let (forward_sender, mut forward_receiver) = futures::channel::mpsc::unbounded();
                    
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
                    let connection_result = connection::initialize_connection().await;

                    match connection_result {
                        Ok((_websocket_connection, mut web_api)) => {
                            let (_host_response_sender, mut host_response_receiver) =
                                futures::channel::mpsc::unbounded::<Result<freenet_stdlib::client_api::HostResponse, String>>();

                            info!("FreenetApi initialized with WebSocket URL");

                            // Set up room subscriptions and updates
                            subscription::setup_room_subscriptions(request_sender_clone.clone());

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
                                                            processor::process_get_response(key, state.to_vec());
                                                        },
                                                        ContractResponse::UpdateNotification { key, update } => {
                                                            processor::process_update_notification(key, update);
                                                        },
                                                        _ => {}
                                                    }
                                                },
                                                HostResponse::Ok => {
                                                    processor::process_ok_response();
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
                            let _ = futures_timer::Delay::new(std::time::Duration::from_secs(3)).await;

                            // Break out of the current connection context
                            break;
                        },
                        Err(e) => {
                            // Connection failed, wait before retrying
                            error!("Failed to establish WebSocket connection: {}", e);
                            *SYNC_STATUS.write() = SyncStatus::Error(format!("Connection failed: {}", e));
                            let _ = futures_timer::Delay::new(std::time::Duration::from_secs(5)).await;

                            // Continue to retry
                            continue;
                        }
                    }
                }
            }
        });
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
        let parameters = subscription::prepare_chat_room_parameters(room_owner);
        let contract_key = subscription::generate_contract_key(parameters);
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
        subscription::request_room_state(&mut self.sender.request_sender, room_owner).await
    }
}
