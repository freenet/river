use common::room_state::ChatRoomParametersV1;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::{client_api::{ClientError, ClientRequest, ContractRequest, HostResponse}, prelude::{ContractCode, ContractInstanceId, ContractKey, Parameters}};
use freenet_stdlib::client_api::WebApi;

use crate::{constants::ROOM_CONTRACT_WASM, util::to_cbor_vec};
use futures::sink::SinkExt;

pub struct FreenetApi<'a> {
    pub api: WebApi,
    pub requests: futures::channel::mpsc::UnboundedReceiver<ClientRequest<'a>>,
    pub host_responses: futures::channel::mpsc::UnboundedSender<HostResponse>,
}

impl FreenetApi<'_> {
    pub fn new() -> Self {
        let conn = web_sys::WebSocket::new(
            "ws://localhost:50509/contract/command?encodingProtocol=native",
        )
        .unwrap();
        let (send_host_responses, host_responses) = futures::channel::mpsc::unbounded();
        let (send_half, requests) =
            futures::channel::mpsc::unbounded::<freenet_stdlib::client_api::ClientRequest>();
        let result_handler = move |result: Result<HostResponse, ClientError>| {
            let mut send_host_responses_clone = send_host_responses.clone();
            let _ = wasm_bindgen_futures::future_to_promise(async move {
                send_host_responses_clone
                    .send(result)
                    .await
                    .expect("channel open");
                Ok(wasm_bindgen::JsValue::NULL)
            });
        };
        let (tx, rx) = futures::channel::oneshot::channel();
        let onopen_handler = move || {
            let _ = tx.send(());
        };
        let mut api = WebApi::start(
            conn,
            result_handler,
            |err| {
                log::error!("host error: {}", err);
            },
            onopen_handler,
        );
        todo!()
        //Self {
        //    api,
        //    requests: requests,
        //    host_responses: host_responses,
        // }
    }

    pub fn subscribe(&mut self, room_owner: &VerifyingKey) {
        let parameters = ChatRoomParametersV1 { owner: *room_owner };
        let parameters = to_cbor_vec(&parameters);
        let parameters: Parameters = parameters.into();
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_instance_id =
            ContractInstanceId::from_params_and_code(parameters, contract_code);
        let contract_key = ContractKey::from(contract_instance_id);
        let request = ContractRequest::Subscribe {key : contract_key, summary : None };
       // self.api.send_request(request);
    }
}
