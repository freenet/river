use std::collections::HashSet;
use dioxus::prelude::*;
use futures::StreamExt;
use common::room_state::ChatRoomParametersV1;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::{
    client_api::{ClientError, ClientRequest, ContractRequest, HostResponse},
    prelude::{ContractCode, ContractInstanceId, ContractKey, Parameters},
};
use freenet_stdlib::client_api::WebApi;
use crate::{constants::ROOM_CONTRACT_WASM, util::to_cbor_vec};

#[derive(Clone, Debug)]
pub enum SyncStatus {
    Connecting,
    Connected,
    Syncing,
    Error(String),
}

pub static SYNC_STATUS: GlobalSignal<SyncStatus> = Signal::global(|| SyncStatus::Connecting);
use futures::sink::SinkExt;

const WEBSOCKET_URL: &str = "ws://localhost:50509/contract/command?encodingProtocol=native";

/// FreenetApi is a wrapper around the Web API to interact with Freenet.
pub struct FreenetApiSynchronizer<'a> {

    /// The Web API for communicating with Freenet.
    pub web_api: WebApi,

    /// Receiver for incoming client requests (e.g., Subscribe, Unsubscribe).
    pub client_request_receiver: futures::channel::mpsc::UnboundedReceiver<ClientRequest<'a>>,

    /// Sender for sending host responses back to the client.
    pub host_response_sender: futures::channel::mpsc::UnboundedSender<Result<HostResponse, ClientError>>,

    /// Contracts that we've already subscribed to via the API
    pub subscribed_contracts : HashSet<ContractKey>,
}

impl<'a> FreenetApiSynchronizer<'a> {
    /// Starts the Freenet API syncrhonizer.
    pub fn start() -> Self {
        let subscribed_contracts = HashSet::new();
        
        // Start the sync coroutine
        use_coroutine(move |mut rx| {
            async move {
                let mut pinned = std::pin::pin!(SYNC_STATUS.write());
                pinned.set(SyncStatus::Connecting);
                
                let websocket_connection = match web_sys::WebSocket::new(WEBSOCKET_URL) {
                    Ok(ws) => ws,
                    Err(e) => {
                        let mut pinned = std::pin::pin!(SYNC_STATUS.write());
                        pinned.set(SyncStatus::Error(format!("Failed to connect: {:?}", e)));
                        return;
                    }
                };

                let (host_response_sender, mut host_response_receiver) = 
                    futures::channel::mpsc::unbounded();

                let mut web_api = WebApi::start(
                    websocket_connection,
                    move |result| {
                        let mut sender = host_response_sender.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            if let Err(e) = sender.send(result).await {
                                log::error!("Failed to send response: {}", e);
                            }
                        });
                    },
                    |error| {
                        let mut pinned = std::pin::pin!(SYNC_STATUS.write());
                        pinned.set(SyncStatus::Error(error.to_string()));
                    },
                    || {
                        let mut pinned = std::pin::pin!(SYNC_STATUS.write());
                        pinned.set(SyncStatus::Connected);
                    },
                );

                log::info!("FreenetApi initialized");

                // Main event loop
                loop {
                    futures::select! {
                        // Handle incoming client requests
                        msg = rx.next() => {
                            if let Some(request) = msg {
                                let mut pinned = std::pin::pin!(SYNC_STATUS.write());
                                pinned.set(SyncStatus::Syncing);
                                if let Err(e) = web_api.send(request).await {
                                    let mut pinned = std::pin::pin!(SYNC_STATUS.write());
                                    pinned.set(SyncStatus::Error(e.to_string()));
                                }
                            }
                        }
                        
                        // Handle responses from the host
                        response = host_response_receiver.next() => {
                            if let Some(Ok(_response)) = response {
                                // Process the response and update UI state
                                let mut pinned = std::pin::pin!(SYNC_STATUS.write());
                                pinned.set(SyncStatus::Connected);
                            }
                        }
                    }
                }
            }
        });

        Self {
            web_api: WebApi::start(
                web_sys::WebSocket::new(WEBSOCKET_URL).unwrap(),
                |_| {},
                |_| {},
                || {},
            ),
            client_request_receiver: futures::channel::mpsc::unbounded().1,
            host_response_sender: futures::channel::mpsc::unbounded().0,
            subscribed_contracts,
        }
    }

    fn prepare_chat_room_parameters(room_owner: &VerifyingKey) -> Parameters {
        let chat_room_params = ChatRoomParametersV1 { owner: *room_owner };
        to_cbor_vec(&chat_room_params).into()
    }

    fn generate_contract_key(parameters: Parameters) -> ContractKey {
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let instance_id = ContractInstanceId::from_params_and_code(parameters, contract_code);
        ContractKey::from(instance_id)
    }

    /// Subscribes to a chat room owned by the specified room owner.
    pub fn subscribe(&mut self, room_owner: &VerifyingKey) {
        log::info!("Subscribing to chat room owned by {:?}", room_owner);
        let parameters = Self::prepare_chat_room_parameters(room_owner);
        let contract_key = Self::generate_contract_key(parameters);
        let _subscribe_request = ContractRequest::Subscribe {
            key: contract_key,
            summary: None,
        };
        // self.web_api.send_request(subscribe_request);
    }
}
