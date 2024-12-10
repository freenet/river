use super::{room_list::RoomList, conversation::Conversation, members::MemberList};
use crate::components::room_list::edit_room_modal::EditRoomModal;
use crate::constants::ROOM_CONTRACT_WASM;
use crate::util::to_cbor_vec;
use common::room_state::ChatRoomParametersV1;
use dioxus::prelude::*;
use document::Stylesheet;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::{ContractCode, ContractInstanceId, Parameters};
use futures::SinkExt;
use common::room_state::member::MemberId;
use freenet_stdlib::client_api::{ClientError, HostResponse, ClientRequest};
use freenet_stdlib::client_api::WebApi;
use crate::components::members::member_info_modal::MemberInfoModal;
use crate::room_data::{CurrentRoom, Rooms};

pub fn App() -> Element {
    use_context_provider(||
        Signal::new(initial_rooms())
    );
    use_context_provider(|| Signal::new(CurrentRoom { owner_key: None }));
    use_context_provider(|| Signal::new(MemberInfoModalSignal { member: None }));
    use_context_provider(|| Signal::new(EditRoomModalSignal { room: None }));
    use_context_provider(|| Signal::new(CreateRoomModalSignal { show: false }));

    //connect_to_freenet();
    

    rsx! {
        Stylesheet { href: asset!("./assets/bulma.min.css") }
        Stylesheet { href: asset!("./assets/main.css") }
        Stylesheet { href: asset!("./assets/fontawesome/css/all.min.css") }
        div { class: "chat-container",
            RoomList {}
            Conversation {}
            MemberList {}
        }
        EditRoomModal {}
        MemberInfoModal {}

    }
}

#[cfg(not(feature = "example-data"))]
fn initial_rooms() -> Rooms {
    Rooms {
        map: std::collections::HashMap::new(),
    }
}

pub struct FreenetApi<'a> {
    pub api: WebApi,
    pub requests: futures::channel::mpsc::UnboundedReceiver<ClientRequest<'a>>,
    pub host_responses: futures::channel::mpsc::UnboundedSender<HostResponse>,
}

impl FreenetApi<'_> {

    pub fn new() -> Self {
        let conn = web_sys::WebSocket::new(
            "ws://localhost:50509/contract/command?encodingProtocol=native",
        ).unwrap();
        let (send_host_responses, host_responses) = futures::channel::mpsc::unbounded();
        let (send_half, requests) = futures::channel::mpsc::unbounded::<freenet_stdlib::client_api::ClientRequest>();
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

    pub fn subscribe(room_owner : &VerifyingKey) {
        let parameters = ChatRoomParametersV1 {
            owner: *room_owner,
        };
        let parameters = to_cbor_vec(&parameters);
        let parameters : Parameters = parameters.into();
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let contract_instance_id = ContractInstanceId::from_params_and_code(parameters, contract_code);
    }
}

#[cfg(feature = "example-data")]
fn initial_rooms() -> Rooms {
    crate::example_data::create_example_rooms()
}

pub struct EditRoomModalSignal {
    pub room : Option<VerifyingKey>,
}

pub struct CreateRoomModalSignal {
    pub show: bool,
}

pub struct MemberInfoModalSignal {
    pub member: Option<MemberId>
}
