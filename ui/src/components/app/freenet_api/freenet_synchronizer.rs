use super::connection_manager::ConnectionManager;
use super::error::SynchronizerError;
use super::response_handler::ResponseHandler;
use super::room_synchronizer::RoomSynchronizer;
use crate::components::app::{SYNC_STATUS, WEB_API};
use crate::util::sleep;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{HostResponse, WebApi};
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use river_common::room_state::member::AuthorizedMember;
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
                                if let Some(web_api) = &mut *WEB_API.write() {
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
                        if let Err(e) = response_handler
                            .handle_api_response(response.unwrap())
                            .await
                        {
                            error!("Error handling API response: {}", e);
                        }
                    }
                    SynchronizerMessage::AcceptInvitation {
                        owner_vk,
                        authorized_member,
                        invitee_signing_key,
                        nickname,
                    } => {
                        info!("Processing invitation acceptance");
                        if let Err(e) = response_handler
                            .get_room_synchronizer_mut()
                            .create_room_from_invitation(
                                owner_vk,
                                authorized_member,
                                invitee_signing_key,
                                nickname,
                            )
                            .await
                        {
                            error!("Failed to create room from invitation: {}", e);
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
