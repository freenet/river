use super::connection_manager::ConnectionManager;
use super::error::SynchronizerError;
use super::response_handler::ResponseHandler;
use super::room_synchronizer::RoomSynchronizer;
use crate::room_data::Rooms;
use crate::util::sleep;
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
    // Room state signal for effect tracking
    rooms: Signal<Rooms>,
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
            rooms,
        }
    }

    pub async fn start(&mut self) {
        info!("Starting FreenetSynchronizer");
        
        // Take ownership of the receiver
        let mut message_rx = self.message_rx.take().expect("Message receiver already taken");
        let message_tx = self.message_tx.clone();
        
        info!("Setting up message processing loop");
        
        // Move the components out of self to use in the async task
        info!("Preparing connection manager");
        let mut connection_manager = ConnectionManager::new(self.connection_manager.get_status_signal().clone());
        std::mem::swap(&mut connection_manager, &mut self.connection_manager);
        
        info!("Preparing response handler");
        let mut response_handler = ResponseHandler::new(self.response_handler.take_room_synchronizer());
        
        // Create a separate clone for room state monitoring
        let rooms = self.rooms.clone();
        let process_tx = message_tx.clone();
        
        // Set up a separate task to monitor room changes
        spawn_local(async move {
            info!("Starting room state monitor");
            let mut last_len = 0;
            
            loop {
                // Check if rooms have changed
                let current_len = rooms.read().map.len();
                if current_len != last_len {
                    info!("Rooms state changed, checking for sync needs (count: {})", current_len);
                    last_len = current_len;
                    
                    // Send a message to process rooms
                    if let Err(e) = process_tx.unbounded_send(SynchronizerMessage::ProcessRooms) {
                        error!("Failed to send ProcessRooms message: {}", e);
                        break;
                    }
                }
                
                // Sleep to avoid busy waiting
                sleep(std::time::Duration::from_millis(500)).await;
            }
            
            warn!("Room state monitor ended");
        });
        
        info!("Starting message processing loop");
        spawn_local(async move {
            info!("Sending initial Connect message");
            // Start connection
            if let Err(e) = message_tx.unbounded_send(SynchronizerMessage::Connect) {
                error!("Failed to send Connect message: {}", e);
            }
            
            info!("Entering message loop");
            // Process messages
            while let Some(msg) = message_rx.next().await {
                match msg {
                    SynchronizerMessage::ProcessRooms => {
                        info!("Processing rooms");
                        if let Some(web_api) = connection_manager.get_api_mut() {
                            if let Err(e) = response_handler.get_room_synchronizer_mut().process_rooms(web_api).await {
                                error!("Error processing rooms: {}", e);
                            }
                        } else {
                            error!("Cannot process rooms: API not initialized");
                        }
                    },
                    SynchronizerMessage::Connect => {
                        info!("Connecting to Freenet");
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
                        info!("Received API response");
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
        
        info!("FreenetSynchronizer start method completed");
    }

    // Helper method to send a connect message
    pub fn connect(&self) {
        if let Err(e) = self.message_tx.unbounded_send(SynchronizerMessage::Connect) {
            error!("Failed to send Connect message: {}", e);
        }
    }
}

