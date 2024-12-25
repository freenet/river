use std::collections::HashSet;
use futures::StreamExt;
use common::room_state::ChatRoomParametersV1;
use dioxus::prelude::{Global, GlobalSignal, UnboundedSender, use_coroutine, use_context, Signal, Writable, use_effect, Readable};
use freenet_scaffold::ComposableState;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest, HostResponse, ContractResponse},
    prelude::{ContractCode, ContractInstanceId, ContractKey, Parameters},
};
use freenet_stdlib::client_api::WebApi;
use crate::{constants::ROOM_CONTRACT_WASM, util::to_cbor_vec, room_data::Rooms};

#[derive(Clone, Debug)]
pub enum SyncStatus {
    Connecting,
    Connected,
    Syncing,
    Error(String),
}

use futures::sink::SinkExt;

static SYNC_STATUS: GlobalSignal<SyncStatus> = Global::new(|| SyncStatus::Connecting);

const WEBSOCKET_URL: &str = "ws://localhost:50509/contract/command?encodingProtocol=native";

#[derive(Clone)]
pub struct FreenetApiSender {
    request_sender: UnboundedSender<ClientRequest<'static>>,
}

/// FreenetApi is a wrapper around the Web API to interact with Freenet.
pub struct FreenetApiSynchronizer {
    /// The Web API for communicating with Freenet.
    pub web_api: WebApi,

    /// Contracts that we've already subscribed to via the API
    pub subscribed_contracts: HashSet<ContractKey>,
    
    /// Sender that can be cloned and shared
    pub sender: FreenetApiSender,
}

impl FreenetApiSynchronizer {
    /// Starts the Freenet API syncrhonizer.
    pub fn start() -> Self {
        let subscribed_contracts = HashSet::new();
        let (request_sender, _request_receiver) = futures::channel::mpsc::unbounded();
        let sender_for_struct = request_sender.clone();
        
        // Start the sync coroutine 
        use_coroutine(move |mut rx| {
            let request_sender_clone = request_sender.clone();
            async move {
                *SYNC_STATUS.write() = SyncStatus::Connecting;
                
                let websocket_connection = match web_sys::WebSocket::new(WEBSOCKET_URL) {
                    Ok(ws) => ws,
                    Err(e) => {
                        *SYNC_STATUS.write() = SyncStatus::Error(format!("Failed to connect: {:?}", e));
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
                        *SYNC_STATUS.write() = SyncStatus::Error(error.to_string());
                    },
                    || {
                        *SYNC_STATUS.write() = SyncStatus::Connected;
                    },
                );

                log::info!("FreenetApi initialized");
                
                // Watch for changes to Rooms signal
                let rooms = use_context::<Signal<Rooms>>();
                let request_sender_clone = request_sender.clone();
                
                use_effect(move || {
                    {
                        let rooms_ref = rooms.read();
                        for room in rooms_ref.map.values() {
                            let state_bytes = to_cbor_vec(&room.room_state);
                            let update_request = ContractRequest::Update {
                                key: room.contract_key,
                                data: freenet_stdlib::prelude::UpdateData::State(state_bytes.into()),
                            };
                            let mut sender = request_sender_clone.clone();
                            wasm_bindgen_futures::spawn_local(async move {
                                if let Err(e) = sender.send(update_request.into()).await {
                                    log::error!("Failed to send room update: {}", e);
                                }
                            });
                        }
                    }
                });

                // Main event loop
                loop {
                    futures::select! {
                        // Handle incoming client requests
                        msg = rx.next() => {
                            if let Some(request) = msg {
                                *SYNC_STATUS.write() = SyncStatus::Syncing;
                                if let Err(e) = web_api.send(request).await {
                                    *SYNC_STATUS.write() = SyncStatus::Error(e.to_string());
                                }
                            }
                        }
                        
                        // Handle responses from the host
                        response = host_response_receiver.next() => {
                            if let Some(Ok(response)) = response {
                                match response {
                                    HostResponse::ContractResponse(contract_response) => {
                                        match contract_response {
                                            ContractResponse::GetResponse { key, state, .. } => {
                                                // Update rooms with received state
                                                if let Ok(room_state) = ciborium::from_reader(state.as_ref()) {
                                                    let mut rooms = use_context::<Signal<Rooms>>();
                                                    let mut rooms = rooms.write();
                                                    if let Some(room_data) = rooms.map.values_mut().find(|r| r.contract_key == key) {
                                                        let current_state = room_data.room_state.clone();
                                                        if let Err(e) = room_data.room_state.merge(
                                                            &current_state,
                                                            &room_data.parameters(),
                                                            &room_state
                                                        ) {
                                                            log::error!("Failed to merge room state: {}", e);
                                                            *SYNC_STATUS.write() = SyncStatus::Error(e);
                                                        }
                                                    }
                                                } else {
                                                    log::error!("Failed to decode room state");
                                                }
                                            },
                                            ContractResponse::UpdateNotification { key, update } => {
                                                // Handle incremental updates
                                                let mut rooms = use_context::<Signal<Rooms>>();
                                                let mut rooms = rooms.write();
                                                let key_bytes: [u8; 32] = key.id().as_bytes().try_into().expect("Invalid key length");
                                                if let Some(room_data) = rooms.map.get_mut(&VerifyingKey::from_bytes(&key_bytes).expect("Invalid key bytes")) {
                                                    if let Ok(delta) = ciborium::from_reader(update.unwrap_delta().as_ref()) {
                                                        let current_state = room_data.room_state.clone();
                                                        if let Err(e) = room_data.room_state.apply_delta(
                                                            &current_state,
                                                            &room_data.parameters(),
                                                            &Some(delta)
                                                        ) {
                                                            log::error!("Failed to apply delta: {}", e);
                                                            *SYNC_STATUS.write() = SyncStatus::Error(e);
                                                        }
                                                    }
                                                }
                                            },
                                            _ => {}
                                        }
                                    },
                                    HostResponse::Ok => {
                                        *SYNC_STATUS.write() = SyncStatus::Connected;
                                    },
                                    _ => {}
                                }
                            } else if let Some(Err(e)) = response {
                                *SYNC_STATUS.write() = SyncStatus::Error(e.to_string());
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
            subscribed_contracts,
            sender: FreenetApiSender { request_sender: sender_for_struct },
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
    pub async fn subscribe(&mut self, room_owner: &VerifyingKey) {
        log::info!("Subscribing to chat room owned by {:?}", room_owner);
        let parameters = Self::prepare_chat_room_parameters(room_owner);
        let contract_key = Self::generate_contract_key(parameters);
        let subscribe_request = ContractRequest::Subscribe {
            key: contract_key,
            summary: None,
        };
        self.sender.request_sender.send(subscribe_request.into()).await.expect("Unable to send request");
    }
}
