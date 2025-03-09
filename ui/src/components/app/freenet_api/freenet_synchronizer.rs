use super::connection_manager::ConnectionManager;
use super::error::SynchronizerError;
use super::response_handler::ResponseHandler;
use super::room_synchronizer::RoomSynchronizer;
use crate::components::app::WEB_API;
use crate::room_data::Rooms;
use crate::util::sleep;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{HostResponse, WebApi};
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use river_common::room_state::member::AuthorizedMember;
use std::ops::{Deref, DerefMut};
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
    // Channel for sending messages to the synchronizer
    pub message_tx: UnboundedSender<SynchronizerMessage>,
    message_rx: Option<UnboundedReceiver<SynchronizerMessage>>,
    // Components
    connection_manager: ConnectionManager,
    response_handler: ResponseHandler,
    // Track if connection is ready
    connection_ready: bool,
}

#[derive(Debug, Clone, PartialEq)]
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
    pub fn new() -> Self {
        let (message_tx, message_rx) = unbounded();

        // Create components with their own copies of the signals
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

    // Get a clone of the message sender
    pub fn get_message_sender(&self) -> UnboundedSender<SynchronizerMessage> {
        self.message_tx.clone()
    }

    pub async fn start(&mut self) {
        info!("Starting FreenetSynchronizer");

        // Only start if we haven't already
        if self.message_rx.is_none() {
            info!("FreenetSynchronizer is already running, ignoring start request");
            return;
        }

        // Take ownership of the receiver
        let mut message_rx = self
            .message_rx
            .take()
            .expect("Message receiver already taken");
        let message_tx = self.message_tx.clone();

        info!("Setting up message processing loop");

        // Move the components out of self to use in the async task
        info!("Preparing connection manager");
        // Get a reference to the connection manager's status signal
        // Create a new connection manager with the same status signal
        let mut connection_manager = ConnectionManager::new();

        info!("Preparing response handler");
        // Get a reference to the response handler's room synchronizer
        let room_synchronizer_ref = self.response_handler.get_room_synchronizer();
        // Create a new response handler that uses the same room synchronizer
        let mut response_handler =
            ResponseHandler::new_with_shared_synchronizer(room_synchronizer_ref);

        // Note: We've removed the periodic room check here.
        // Room synchronization is now triggered by use_effect in app.rs

        info!("Starting message processing loop");
        spawn_local(async move {
            info!("Sending initial Connect message");
            // Start connection
            if let Err(e) = message_tx.unbounded_send(SynchronizerMessage::Connect) {
                error!("Failed to send Connect message: {}", e);
            }
            let web_api: &mut Option<WebApi> = WEB_API.write().deref_mut();
            info!("Entering message loop");
            // Process messages
            while let Some(msg) = message_rx.next().await {
                match msg {
                    SynchronizerMessage::ProcessRooms => {
                        info!("Processing rooms request received");

                        // Check if connection is ready before processing
                        if !connection_manager.is_connected() {
                            info!("Connection not ready, deferring room processing and attempting to connect");
                            if let Err(e) = message_tx.unbounded_send(SynchronizerMessage::Connect)
                            {
                                error!("Failed to send Connect message: {}", e);
                            }
                            continue;
                        }

                        info!("Connection is ready, processing rooms");
                        match web_api {
                            Some(web_api) => {
                                info!("Got API reference, processing rooms");
                                if let Err(e) = response_handler
                                    .get_room_synchronizer_mut()
                                    .process_rooms(web_api)
                                    .await
                                {
                                    error!("Error processing rooms: {}", e);
                                } else {
                                    info!("Successfully processed rooms");
                                }
                            }
                            None => {
                                // This shouldn't happen if is_connected() returned true, but handle it anyway
                                error!("Cannot process rooms: API not initialized despite connection being ready");
                                if let Err(e) =
                                    message_tx.unbounded_send(SynchronizerMessage::Connect)
                                {
                                    error!("Failed to send Connect message: {}", e);
                                }
                            }
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

                                // Process rooms to sync them
                                if let Some(web_api) = web_api {
                                    info!("Connection ready, processing rooms after successful connection");
                                    if let Err(e) = response_handler
                                        .get_room_synchronizer_mut()
                                        .process_rooms(web_api)
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
                                // Schedule a reconnect attempt
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
                            Ok(api_response) => {
                                info!("API response is OK: {:?}", api_response);
                                if let Err(e) = response_handler
                                    .handle_api_response(api_response, &mut web_api.unwrap())
                                    .await
                                {
                                    error!("Error handling API response: {}", e);
                                }
                            }
                            Err(e) => {
                                error!("Error in API response: {}", e);
                                // Log more details about the error
                                error!("Error type: {:?}", e);
                                if e.to_string().contains("not found in store") {
                                    info!("This appears to be a 'contract not found' error, which may be expected for new contracts");
                                }
                            }
                        }
                    }
                    SynchronizerMessage::AcceptInvitation {
                        owner_vk,
                        authorized_member,
                        invitee_signing_key,
                        nickname,
                    } => {
                        info!("Processing invitation acceptance for room: {:?}", owner_vk);

                        match response_handler
                            .get_room_synchronizer_mut()
                            .create_room_from_invitation(
                                owner_vk,
                                authorized_member,
                                invitee_signing_key,
                                nickname,
                                &mut web_api.unwrap(),
                            )
                            .await
                        {
                            Ok(_) => {
                                info!("Successfully created room from invitation");

                                // We need to update the pending invites status
                                info!("Room created successfully, updating pending invites");

                                // Send a message to the UI thread to update the pending invites
                                spawn_local({
                                    let owner_vk = owner_vk.clone();
                                    async move {
                                        // Use a small delay to ensure the room is fully created
                                        sleep(Duration::from_millis(500)).await;

                                        // This will be picked up by the UI to update the status
                                        let window = web_sys::window().expect("No window found");

                                        // Create a custom event with the room key as detail
                                        let key_hex = owner_vk
                                            .to_bytes()
                                            .to_vec()
                                            .iter()
                                            .map(|b| format!("{:02x}", b))
                                            .collect::<String>();

                                        // Create event using the standard web_sys API
                                        let event =
                                            web_sys::CustomEvent::new("river-invitation-accepted")
                                                .expect("Failed to create event");

                                        // Set the detail property using JS
                                        js_sys::Reflect::set(
                                            &event,
                                            &wasm_bindgen::JsValue::from_str("detail"),
                                            &wasm_bindgen::JsValue::from_str(&key_hex),
                                        )
                                        .expect("Failed to set event detail");

                                        window
                                            .dispatch_event(&event)
                                            .expect("Failed to dispatch event");
                                    }
                                });
                            }
                            Err(e) => {
                                error!("Failed to create room from invitation: {}", e);

                                // We can't use use_context here as we're not in a component
                                // The UI will handle updating the pending invites status
                                error!("Room creation failed: {}", e);
                            }
                        }
                    }
                }
            }

            warn!("Synchronizer message loop ended");
        });

        info!("FreenetSynchronizer start method completed");
    }

    // Helper method to send a connect message
    pub fn connect(&self) {
        if let Err(e) = self.message_tx.unbounded_send(SynchronizerMessage::Connect) {
            error!("Failed to send Connect message: {}", e);
        }
    }

    // Check if the synchronizer is running
    pub fn is_running(&self) -> bool {
        self.message_rx.is_none()
    }

    // Check if we're connected to Freenet
    pub fn is_connected(&self) -> bool {
        matches!(
            *self.connection_manager.get_status_signal().read(),
            SynchronizerStatus::Connected
        )
    }
}
