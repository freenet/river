use std::collections::HashSet;
use common::room_state::ChatRoomParametersV1;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::{
    client_api::{ClientError, ClientRequest, ContractRequest, HostResponse},
    prelude::{ContractCode, ContractInstanceId, ContractKey, Parameters},
};
use freenet_stdlib::client_api::WebApi;
use crate::{constants::ROOM_CONTRACT_WASM, util::to_cbor_vec};
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
        let websocket_connection = web_sys::WebSocket::new(WEBSOCKET_URL)
            .expect("Failed to create WebSocket connection");

        let (host_response_sender, host_response_receiver) = futures::channel::mpsc::unbounded::<Result<HostResponse, ClientError>>();
        let (client_request_sender, client_request_receiver) =
            futures::channel::mpsc::unbounded::<ClientRequest>();

        let result_handler = {
            let host_response_sender = host_response_sender.clone();
            move |result: Result<HostResponse, ClientError>| {
                let mut cloned_sender = host_response_sender.clone();
                let _ = wasm_bindgen_futures::future_to_promise(async move {
                    if let Err(err) = cloned_sender.send(result).await {
                        log::error!("Failed to send host response: {}", err);
                    }
                    Ok(wasm_bindgen::JsValue::NULL)
                });
            }
        };

        let (on_open_sender, on_open_receiver) = futures::channel::oneshot::channel();
        let on_open_handler = move || {
            let _ = on_open_sender.send(());
        };

        let web_api = WebApi::start(
            websocket_connection,
            result_handler,
            |error| log::error!("Error from WebSocket host: {}", error),
            on_open_handler,
        );

        log::info!("FreenetApi initialized");

        Self {
            web_api,
            client_request_receiver,
            host_response_sender,
            subscribed_contracts: HashSet::new(),
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
        let subscribe_request = ContractRequest::Subscribe {
            key: contract_key,
            summary: None,
        };
        // self.web_api.send_request(subscribe_request);
    }
}