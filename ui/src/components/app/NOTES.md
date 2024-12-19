## Relevant Context

Please read https://dioxuslabs.com/learn/0.6/reference/use_coroutine as it will probably be relevant.

These notes are intended to help with the implementation of freenet_api.rs, which is the interface between the River group chat
app and a local Freenet node, which it communicates with via a websocket API.

See member_info_modal.rs for an example of how the Rooms are retrieved using use_context::<Signal<Rooms>>() and how they can
be modified by creating an applying a "delta". The purpose of the freenet_api.rs is to send modifed room state to the Freenet
node and to receive updates to the room state from the Freenet node.

# This is a section of code that illustrates how to use the Freenet client websocket API, this is NOT part of the River codebase

```rust
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::OnceLock};

#[cfg(feature = "use-node")]
use dioxus::prelude::UseSharedState;
use dioxus::prelude::{UnboundedReceiver, UnboundedSender};
use freenet_aft_interface::{TokenAllocationSummary, TokenDelegateMessage};
use freenet_stdlib::client_api::{ClientError, ClientRequest, HostResponse};
use futures::SinkExt;

use crate::app::{ContractType, InboxController};
use crate::DynError;

type ClientRequester = UnboundedSender<ClientRequest<'static>>;
type HostResponses = UnboundedReceiver<Result<HostResponse, ClientError>>;

pub(crate) type NodeResponses = UnboundedSender<AsyncActionResult>;

pub(crate) static WEB_API_SENDER: OnceLock<WebApiRequestClient> = OnceLock::new();

#[cfg(feature = "use-node")]
struct WebApi {
    requests: UnboundedReceiver<ClientRequest<'static>>,
    host_responses: HostResponses,
    client_errors: UnboundedReceiver<AsyncActionResult>,
    send_half: ClientRequester,
    error_sender: NodeResponses,
    api: freenet_stdlib::client_api::WebApi,
    connecting: Option<futures::channel::oneshot::Receiver<()>>,
}

#[cfg(not(feature = "use-node"))]
struct WebApi {}

impl WebApi {
    #[cfg(not(feature = "use-node"))]
    fn new() -> Result<Self, String> {
        Ok(Self {})
    }

    #[cfg(all(not(target_family = "wasm"), feature = "use-node"))]
    fn new() -> Result<Self, String> {
        unimplemented!()
    }

    #[cfg(all(target_family = "wasm", feature = "use-node"))]
    fn new() -> Result<Self, String> {
        use futures::{SinkExt, StreamExt};
        let conn = web_sys::WebSocket::new(
            "ws://localhost:50509/contract/command?encodingProtocol=native",
        )
        .unwrap();
        let (send_host_responses, host_responses) = futures::channel::mpsc::unbounded();
        let (send_half, requests) = futures::channel::mpsc::unbounded();
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
            crate::log::debug!("connected to websocket");
        };
        let mut api = freenet_stdlib::client_api::WebApi::start(
            conn,
            result_handler,
            |err| {
                crate::log::error(format!("host error: {err}"), None);
            },
            onopen_handler,
        );
        let (error_sender, client_errors) = futures::channel::mpsc::unbounded();

        Ok(Self {
            requests,
            host_responses,
            client_errors,
            send_half,
            error_sender,
            api,
            connecting: Some(rx),
        })
    }

    #[cfg(feature = "use-node")]
    fn sender_half(&self) -> WebApiRequestClient {
        WebApiRequestClient {
            sender: self.send_half.clone(),
            responses: self.error_sender.clone(),
        }
    }

    #[cfg(not(feature = "use-node"))]
    fn sender_half(&self) -> WebApiRequestClient {
        WebApiRequestClient
    }
}

#[cfg(feature = "use-node")]
#[derive(Clone, Debug)]
pub(crate) struct WebApiRequestClient {
    sender: ClientRequester,
    responses: NodeResponses,
}

#[cfg(not(feature = "use-node"))]
#[derive(Clone, Debug)]
pub(crate) struct WebApiRequestClient;

impl WebApiRequestClient {
    #[cfg(feature = "use-node")]
    pub async fn send(
        &mut self,
        request: freenet_stdlib::client_api::ClientRequest<'static>,
    ) -> Result<(), freenet_stdlib::client_api::Error> {
        self.sender
            .send(request)
            .await
            .map_err(|_| freenet_stdlib::client_api::Error::ChannelClosed)?;
        self.sender.flush().await.unwrap();
        Ok(())
    }

    #[cfg(not(feature = "use-node"))]
    pub async fn send(
        &mut self,
        request: freenet_stdlib::client_api::ClientRequest<'static>,
    ) -> Result<(), freenet_stdlib::client_api::Error> {
        tracing::debug!(?request, "emulated request");
        Ok(())
    }
}

#[cfg(feature = "use-node")]
impl From<WebApiRequestClient> for NodeResponses {
    fn from(val: WebApiRequestClient) -> Self {
        val.responses
    }
}

#[cfg(not(feature = "use-node"))]
impl From<WebApiRequestClient> for NodeResponses {
    fn from(_val: WebApiRequestClient) -> Self {
        unimplemented!()
    }
}

#[cfg(feature = "use-node")]
mod contract_api {
    use freenet_stdlib::{client_api::ContractRequest, prelude::*};

    use super::*;

    pub(super) async fn create_contract(
        client: &mut WebApiRequestClient,
        contract_code: &[u8],
        contract_state: impl Into<Vec<u8>>,
        params: &Parameters<'static>,
    ) -> Result<ContractKey, DynError> {
        let contract = ContractContainer::try_from((contract_code.to_vec(), params))?;
        let key = contract.key();
        crate::log::debug!("putting contract {key}");
        let state = contract_state.into().into();
        let request = ContractRequest::Put {
            contract,
            state,
            related_contracts: Default::default(),
        };
        client.send(request.into()).await?;
        Ok(key)
    }
}
```

# This is the browser version of the WebAPI used by the above code, this is not part of the River codebase

```rust
use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::{prelude::Closure, JsCast, JsValue};
use web_sys::{ErrorEvent, MessageEvent};

use super::{client_events::ClientRequest, Error, HostResult};

type Connection = web_sys::WebSocket;

pub struct WebApi {
    conn: Connection,
    error_handler: Box<dyn FnMut(Error) + 'static>,
}

impl WebApi {
    pub fn start<ErrFn>(
        conn: Connection,
        result_handler: impl FnMut(HostResult) + 'static,
        error_handler: ErrFn,
        onopen_handler: impl FnOnce() + 'static,
    ) -> Self
    where
        ErrFn: FnMut(Error) + Clone + 'static,
    {
        let eh = Rc::new(RefCell::new(error_handler.clone()));
        let result_handler = Rc::new(RefCell::new(result_handler));
        let onmessage_callback = Closure::<dyn FnMut(_)>::new(move |e: MessageEvent| {
            // Extract the Blob from the MessageEvent
            let value: JsValue = e.data();
            let blob: web_sys::Blob = value.into();

            // Create a FileReader to read the Blob
            let file_reader = web_sys::FileReader::new().unwrap();

            // Clone FileReader and function references for use in the onloadend_callback
            let fr_clone = file_reader.clone();
            let eh_clone = eh.clone();
            let result_handler_clone = result_handler.clone();

            let onloadend_callback = Closure::<dyn FnMut()>::new(move || {
                let array_buffer = fr_clone
                    .result()
                    .unwrap()
                    .dyn_into::<js_sys::ArrayBuffer>()
                    .unwrap();
                let bytes = js_sys::Uint8Array::new(&array_buffer).to_vec();
                let response: HostResult = match bincode::deserialize(&bytes) {
                    Ok(val) => val,
                    Err(err) => {
                        eh_clone.borrow_mut()(Error::ConnectionError(serde_json::json!({
                            "error": format!("{err}"),
                            "source": "host response deserialization"
                        })));
                        return;
                    }
                };
                result_handler_clone.borrow_mut()(response);
            });

            // Set the FileReader handlers
            file_reader.set_onloadend(Some(onloadend_callback.as_ref().unchecked_ref()));
            file_reader.read_as_array_buffer(&blob).unwrap();
            onloadend_callback.forget();
        });
        conn.set_onmessage(Some(onmessage_callback.as_ref().unchecked_ref()));
        onmessage_callback.forget();

        let mut eh = error_handler.clone();
        let onerror_callback = Closure::<dyn FnMut(_)>::new(move |e: ErrorEvent| {
            let error = format!(
                "error: {file}:{lineno}: {msg}",
                file = e.filename(),
                lineno = e.lineno(),
                msg = e.message()
            );
            eh(Error::ConnectionError(serde_json::json!({
                "error": error, "source": "exec error"
            })));
        });
        conn.set_onerror(Some(onerror_callback.as_ref().unchecked_ref()));
        onerror_callback.forget();

        let onopen_callback = Closure::<dyn FnOnce()>::once(move || {
            onopen_handler();
        });
        // conn.add_event_listener_with_callback("open", onopen_callback.as_ref().unchecked_ref());
        conn.set_onopen(Some(onopen_callback.as_ref().unchecked_ref()));
        onopen_callback.forget();

        // let mut eh = error_handler.clone();
        // let onclose_callback = Closure::<dyn FnOnce()>::once(move || {
        //     tracing::warn!("connection closed");
        //     eh(Error::ConnectionError(
        //         serde_json::json!({ "error": "connection closed", "source": "close" }),
        //     ));
        // });
        // conn.set_onclose(Some(onclose_callback.as_ref().unchecked_ref()));

        conn.set_binary_type(web_sys::BinaryType::Blob);
        WebApi {
            conn,
            error_handler: Box::new(error_handler),
        }
    }

    pub async fn send(&mut self, request: ClientRequest<'static>) -> Result<(), Error> {
        // (self.error_handler)(Error::ConnectionError(serde_json::json!({
        //     "request": format!("{request:?}"),
        //     "action": "sending request"
        // })));
        let send = bincode::serialize(&request)?;
        self.conn.send_with_u8_array(&send).map_err(|err| {
            let err: serde_json::Value = match serde_wasm_bindgen::from_value(err) {
                Ok(e) => e,
                Err(e) => {
                    let e = serde_json::json!({
                        "error": format!("{e}"),
                        "origin": "request serialization",
                        "request": format!("{request:?}"),
                    });
                    (self.error_handler)(Error::ConnectionError(e.clone()));
                    return Error::ConnectionError(e);
                }
            };
            (self.error_handler)(Error::ConnectionError(serde_json::json!({
                "error": err,
                "origin": "request sending",
                "request": format!("{request:?}"),
            })));
            Error::ConnectionError(err)
        })?;
        Ok(())
    }

    pub fn disconnect(self, cause: impl AsRef<str>) {
        let _ = self.conn.close_with_code_and_reason(1000, cause.as_ref());
    }
}
```
