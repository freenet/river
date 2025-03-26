use super::connection_manager::ConnectionManager;
use super::error::SynchronizerError;
use super::response_handler::ResponseHandler;
use super::room_synchronizer::RoomSynchronizer;
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{ROOMS, SYNC_STATUS, WEB_API};
use crate::util::{owner_vk_to_contract_key, sleep};
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::HostResponse;
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use river_common::room_state::member::AuthorizedMember;
use river_common::room_state::member::MemberId;
use std::time::Duration;
use wasm_bindgen_futures::spawn_local;

/// Message types for communicating with the synchronizer
pub enum SynchronizerMessage {
    ProcessRooms,
    Connect,
    ApiResponse(Result<HostResponse, SynchronizerError>),
    AcceptInvitation {
        owner_vk: VerifyingKey,
        authorized_member: AuthorizedMember,
        invitee_signing_key: SigningKey,
        nickname: String,
    },
}

/// Manages synchronization between local room state and Freenet network
pub struct FreenetSynchronizer {
    pub message_tx: UnboundedSender<SynchronizerMessage>,
    message_rx: Option<UnboundedReceiver<SynchronizerMessage>>,
    connection_manager: ConnectionManager,
    response_handler: ResponseHandler,
    connection_ready: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SynchronizerStatus {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}

impl From<SynchronizerError> for SynchronizerStatus {
    fn from(error: SynchronizerError) -> Self {
        SynchronizerStatus::Error(error.to_string())
    }
}

impl FreenetSynchronizer {
    pub fn new() -> Self {
        let (message_tx, message_rx) = unbounded();
        let connection_manager = ConnectionManager::new();
        let room_synchronizer = RoomSynchronizer::new();
        let response_handler = ResponseHandler::new(room_synchronizer);

        info!("Creating new FreenetSynchronizer instance");

        Self {
            message_tx,
            message_rx: Some(message_rx),
            connection_manager,
            response_handler,
            connection_ready: false,
        }
    }

    pub fn get_message_sender(&self) -> UnboundedSender<SynchronizerMessage> {
        self.message_tx.clone()
    }

    pub async fn start(&mut self) {
        info!("Starting FreenetSynchronizer");
        if self.message_rx.is_none() {
            info!("FreenetSynchronizer is already running, ignoring start request");
            return;
        }

        let mut message_rx = self
            .message_rx
            .take()
            .expect("Message receiver already taken");
        let message_tx = self.message_tx.clone();

        info!("Setting up message processing loop");

        let mut connection_manager = ConnectionManager::new();
        let room_synchronizer_ref = self.response_handler.get_room_synchronizer();
        let mut response_handler =
            ResponseHandler::new_with_shared_synchronizer(room_synchronizer_ref);

        info!("Starting message processing loop");
        spawn_local(async move {
            info!("Sending initial Connect message");
            if let Err(e) = message_tx.unbounded_send(SynchronizerMessage::Connect) {
                error!("Failed to send Connect message: {}", e);
            }

            info!("Entering message loop");
            while let Some(msg) = message_rx.next().await {
                match msg {
                    SynchronizerMessage::ProcessRooms => {
                        info!("Processing rooms request received");
                        if !connection_manager.is_connected() {
                            info!("Connection not ready, deferring room processing and attempting to connect");
                            if let Err(e) = message_tx.unbounded_send(SynchronizerMessage::Connect)
                            {
                                error!("Failed to send Connect message: {}", e);
                            }
                            continue;
                        }
                        info!("Connection is ready, processing rooms");
                        if let Err(e) = response_handler
                            .get_room_synchronizer_mut()
                            .process_rooms()
                            .await
                        {
                            error!("Error processing rooms: {}", e);
                        } else {
                            info!("Successfully processed rooms");
                        }
                    }
                    SynchronizerMessage::Connect => {
                        info!("Connecting to Freenet");
                        match connection_manager
                            .initialize_connection(message_tx.clone())
                            .await
                        {
                            Ok(()) => {
                                info!("Connection established successfully");
                                if let Some(_web_api) = &mut *WEB_API.write() {
                                    info!("Processing rooms after successful connection");
                                    if let Err(e) = response_handler
                                        .get_room_synchronizer_mut()
                                        .process_rooms()
                                        .await
                                    {
                                        error!("Error processing rooms after connection: {}", e);
                                    } else {
                                        info!("Successfully processed rooms after connection");
                                    }
                                } else {
                                    error!("API not available after successful connection");
                                }
                            }
                            Err(e) => {
                                error!("Failed to initialize connection: {}", e);
                                let tx = message_tx.clone();
                                spawn_local(async move {
                                    info!("Scheduling reconnection attempt");
                                    sleep(Duration::from_millis(3000)).await;
                                    if let Err(e) = tx.unbounded_send(SynchronizerMessage::Connect)
                                    {
                                        error!("Failed to send reconnect message: {}", e);
                                    }
                                });
                            }
                        }
                    }
                    SynchronizerMessage::ApiResponse(response) => {
                        info!("Received API response");
                        match response {
                            Ok(host_response) => {
                                info!("Processing valid API response: {:?}", host_response);
                                if let Err(e) =
                                    response_handler.handle_api_response(host_response).await
                                {
                                    error!("Error handling API response: {}", e);
                                }
                                info!("Finished processing API response");
                            }
                            Err(e) => {
                                error!("Received error in API response: {}", e);
                                
                                // Special handling for "not supported" errors
                                if e.to_string().contains("not supported") {
                                    warn!("Detected 'not supported' WebSocket operation. This may indicate API version mismatch.");
                                    // Don't immediately reconnect for this specific error as it's likely to recur
                                    *SYNC_STATUS.write() = SynchronizerStatus::Error(
                                        "WebSocket API operation not supported. Check server compatibility.".to_string()
                                    );
                                    continue;
                                }

                                // Log more details about the error
                                if e.to_string().contains("contract")
                                    && e.to_string().contains("not found")
                                {
                                    let error_msg = e.to_string();
                                    if let Some(contract_id) = error_msg
                                        .split_whitespace()
                                        .find(|&word| word.len() > 30 && !word.contains(':'))
                                    {
                                        info!(
                                            "Contract not found error for contract ID: {}",
                                            contract_id
                                        );

                                        // Check if this contract ID exists in our rooms
                                        // Collect room information first to avoid nested borrows
                                        let room_matches: Vec<(VerifyingKey, String)> = {
                                            let rooms = ROOMS.read();
                                            rooms
                                                .map
                                                .iter()
                                                .map(|(room_key, _)| {
                                                    let contract_key =
                                                        owner_vk_to_contract_key(room_key);
                                                    let room_contract_id = contract_key.id();
                                                    (*room_key, room_contract_id.to_string())
                                                })
                                                .collect()
                                        };

                                        let mut found = false;
                                        let mut matching_rooms = Vec::new();

                                        for (room_key, room_contract_id) in &room_matches {
                                            if room_contract_id == contract_id {
                                                info!("Contract ID {} matches room with owner key: {:?}", 
                                                      contract_id, MemberId::from(*room_key));
                                                found = true;
                                                matching_rooms.push(*room_key);
                                            }
                                        }

                                        if found {
                                            // This is likely a race condition where the contract creation
                                            // hasn't completed before we try to access it
                                            // see: https://github.com/freenet/freenet-core/issues/1470
                                            info!("Detected race condition with contract creation. Scheduling retry...");

                                            // Reset the room's sync status to Disconnected so it will be retried
                                            for room_key in &matching_rooms {
                                                info!("Resetting sync status for room {:?} to Disconnected for retry", 
                                                      MemberId::from(*room_key));
                                                SYNC_INFO.write().update_sync_status(
                                                    room_key,
                                                    RoomSyncStatus::Disconnected,
                                                );
                                            }

                                            // Schedule a retry after a delay
                                            let tx = message_tx.clone();
                                            spawn_local(async move {
                                                info!("Waiting before retrying room processing...");
                                                sleep(Duration::from_millis(
                                                    super::constants::POST_PUT_DELAY_MS,
                                                ))
                                                .await;
                                                info!("Retrying room processing after contract not found error");
                                                if let Err(e) = tx.unbounded_send(
                                                    SynchronizerMessage::ProcessRooms,
                                                ) {
                                                    error!("Failed to schedule retry: {}", e);
                                                }
                                            });
                                        } else {
                                            info!(
                                                "Contract ID {} not found in any of our rooms",
                                                contract_id
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    SynchronizerMessage::AcceptInvitation {
                        owner_vk: _,
                        authorized_member: _,
                        invitee_signing_key: _,
                        nickname: _,
                    } => {
                        info!("Processing invitation acceptance");
                        // Instead of creating the room immediately, we'll process it through
                        // the regular room processing flow which will subscribe to the room
                        if let Err(e) = response_handler
                            .get_room_synchronizer_mut()
                            .process_rooms()
                            .await
                        {
                            error!("Failed to process rooms after invitation acceptance: {}", e);
                        }
                    }
                }
            }
            warn!("Synchronizer message loop ended");
        });
    }

    pub fn connect(&self) {
        if let Err(e) = self.message_tx.unbounded_send(SynchronizerMessage::Connect) {
            error!("Failed to send Connect message: {}", e);
        }
    }

    pub fn is_running(&self) -> bool {
        self.message_rx.is_none()
    }

    pub fn is_connected(&self) -> bool {
        matches!(*SYNC_STATUS.read(), SynchronizerStatus::Connected)
    }
}
