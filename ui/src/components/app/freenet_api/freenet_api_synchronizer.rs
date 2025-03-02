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

        *SYNC_STATUS.write() = *sync_status.read();

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
