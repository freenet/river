use super::sync_status::{SyncStatus, SYNC_STATUS};
use super::freenet_api_sender::FreenetApiSender;
use super::constants::WEBSOCKET_URL;
use std::cell::RefCell;
use crate::invites::PendingInvites;
use crate::room_data::Rooms;
use dioxus::logger::tracing::{info, error};
use dioxus::prelude::{Signal, Readable};
use futures::{sink::SinkExt, channel::mpsc::UnboundedSender};
use std::collections::HashSet;
use std::time::Duration;
use freenet_stdlib::{
    client_api::{ClientRequest, HostResponse, WebApi},
    prelude::ContractKey,
};
use futures::future::Either;
use crate::util::sleep;
use wasm_bindgen_futures::spawn_local;
use ed25519_dalek::VerifyingKey;

// Global sender for API requests
thread_local! {
    static GLOBAL_SENDER: RefCell<Option<UnboundedSender<ClientRequest<'static>>>> = RefCell::new(None);
}

// Helper function to access the global sender from anywhere
fn get_global_sender() -> Option<UnboundedSender<ClientRequest<'static>>> {
    GLOBAL_SENDER.with(|sender| sender.borrow().clone())
}

// Helper function to update the global sender
fn update_global_sender(new_sender: UnboundedSender<ClientRequest<'static>>) {
    GLOBAL_SENDER.with(|sender| {
        *sender.borrow_mut() = Some(new_sender);
        info!("Updated global sender");
    });
}

/// Manages synchronization of chat rooms with the Freenet network
#[derive(Clone)]
pub struct FreenetApiSynchronizer {
    pub subscribed_contracts: HashSet<ContractKey>,
    pub sender: FreenetApiSender,
    #[allow(dead_code)]
    ws_ready: bool,
    pub sync_status: Signal<SyncStatus>,
    pub rooms: Signal<Rooms>,
    pub pending_invites: Signal<PendingInvites>,
}

impl FreenetApiSynchronizer {
    pub fn new(
        sync_status: Signal<SyncStatus>,
        rooms: Signal<Rooms>,
        pending_invites: Signal<PendingInvites>,
    ) -> Self {
        let subscribed_contracts = HashSet::new();
        let (request_sender, _request_receiver) = futures::channel::mpsc::unbounded();
        let sender_for_struct = request_sender.clone();

        *SYNC_STATUS.write() = sync_status.read().clone();

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

    pub fn start(&mut self) {
        info!("Starting FreenetApiSynchronizer...");
        spawn_local(async {
            // Initialize connection in the background
            let _ = Self::initialize_connection().await;
        });
    }

    pub async fn request_room_state(&mut self, owner_key: &VerifyingKey) -> Result<(), String> {
        info!("Requesting room state for owner: {:?}", owner_key);
        // This is a placeholder implementation - you'll need to implement the actual logic
        // based on your application's requirements
        Ok(())
    }

    async fn initialize_connection() -> Result<(), String> {
        let (request_sender, _request_receiver) = futures::channel::mpsc::unbounded::<ClientRequest<'static>>();
        let host_response_sender = futures::channel::mpsc::unbounded().0;
        
        let result = Self::initialize_connection_with_sender(host_response_sender).await;
        
        if let Ok((_, _)) = result {
            update_global_sender(request_sender);
            Ok(())
        } else {
            Err("Failed to initialize connection".to_string())
        }
    }

    async fn initialize_connection_with_sender(
        host_response_sender: UnboundedSender<Result<HostResponse, String>>
    ) -> Result<(web_sys::WebSocket, WebApi), String> {
        info!("Starting FreenetApiSynchronizer...");
        info!("Attempting to connect to Freenet node at: {}", WEBSOCKET_URL);
        *SYNC_STATUS.write() = SyncStatus::Connecting;

        let websocket_connection = match web_sys::WebSocket::new(WEBSOCKET_URL) {
            Ok(ws) => {
                info!("WebSocket created successfully");
                let ready_state = ws.ready_state();
                info!("WebSocket initial ready state: {}", ready_state);
                ws
            },
            Err(e) => {
                let error_msg = format!("Failed to connect to WebSocket: {:?}", e);
                error!("{}", error_msg);
                *SYNC_STATUS.write() = SyncStatus::Error(error_msg.clone());
                return Err(error_msg);
            }
        };

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
            Either::Left((true, _)) => Ok((websocket_connection, web_api)),
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
}
