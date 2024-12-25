## Relevant Context

These notes are intended to help with completion of the implementation of freenet_api.rs, which is the interface between the River group chat
app and a local Freenet node, which it communicates with via a websocket API.

See member_info_modal.rs for an example of how the Rooms are retrieved using use_context::<Signal<Rooms>>() and how they can
be modified by creating an applying a "delta".

The goal is to synchronize the rooms in Signal<Rooms> with the rooms in the Freenet node, so that if a Room changes locally then
it is updated in the Freenet node, and vice versa. The room state has a merge function that can be used to merge the local and remote
room states.

This is the complicated part of the implementation, as it involves a lot of async code and the Freenet API is not well documented.

# Questions and Answers

1. **Room State Structure**
Q: I see references to a merge function, but need to see its implementation to understand how states are combined.

A: States are combined using a number of functions defined on the ComposableState trait in scaffold/src/lib.rs which ChatRoomStateV1 
implements. This trait is derived by the #[composable] macro which "zips" together the fields of the struct to implement ComposableState, 
all fields of which must themselves implement ComposableState. The merge function uses a combination of other functions to merge one state
into another. It's required that states can be merged together in any order and the result will be the same, so the merge function must be
commutative and associative - but I don't think this should be a concern for you in completing the implementation of freenet_api.rs.

2. **Signal<Rooms> Interface**
Q: While I see how to access it via use_context, I need to understand the full interface for modifying the Rooms signal.
Q: Are there any specific constraints or patterns for updating the Rooms state?

A: See the nickname_field.rs for an example of how to update the Rooms state. In this case a "delta" is generated, similar to a "diff" in
git, which is then applied to the Rooms state. In the case of freenet_api.rs we will probably be merging an entire state rather than a
delta, but the important thing is it illustrates how to modify the state of a room in Rooms.

3. **Response Handling**
Q: What types of responses can come from the Freenet node for Subscribe/Update requests?
Q: How should different response types be handled and mapped to Room state updates?

```rust
/// A response to a previous [`ClientRequest`]
#[derive(Serialize, Deserialize, Debug)]
#[non_exhaustive]
pub enum HostResponse<T = WrappedState> {
    ContractResponse(#[serde(bound(deserialize = "T: DeserializeOwned"))] ContractResponse<T>),
    DelegateResponse {
        key: DelegateKey,
        values: Vec<OutboundDelegateMsg>,
    },
    /// A requested action which doesn't require an answer was performed successfully.
    Ok,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[non_exhaustive]
pub enum ContractResponse<T = WrappedState> {
    GetResponse {
        key: ContractKey,
        contract: Option<ContractContainer>,
        #[serde(bound(deserialize = "T: DeserializeOwned"))]
        state: T,
    },
    PutResponse {
        key: ContractKey,
    },
    /// Message sent when there is an update to a subscribed contract.
    UpdateNotification {
        key: ContractKey,
        #[serde(deserialize_with = "UpdateData::deser_update_data")]
        update: UpdateData<'static>,
    },
    /// Successful update
    UpdateResponse {
        key: ContractKey,
        #[serde(deserialize_with = "StateSummary::deser_state_summary")]
        summary: StateSummary<'static>,
    },
}

impl<T> From<ContractResponse<T>> for HostResponse<T> {
    fn from(value: ContractResponse<T>) -> HostResponse<T> {
        HostResponse::ContractResponse(value)
    }
}

/// Update notifications for a contract or a related contract.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum UpdateData<'a> {
    State(#[serde(borrow)] State<'a>),
    Delta(#[serde(borrow)] StateDelta<'a>),
    StateAndDelta {
        #[serde(borrow)]
        state: State<'a>,
        #[serde(borrow)]
        delta: StateDelta<'a>,
    },
    RelatedState {
        related_to: ContractInstanceId,
        #[serde(borrow)]
        state: State<'a>,
    },
    RelatedDelta {
        related_to: ContractInstanceId,
        #[serde(borrow)]
        delta: StateDelta<'a>,
    },
    RelatedStateAndDelta {
        related_to: ContractInstanceId,
        #[serde(borrow)]
        state: State<'a>,
        #[serde(borrow)]
        delta: StateDelta<'a>,
    },
}

impl UpdateData<'_> {
    pub fn size(&self) -> usize {
        match self {
            UpdateData::State(state) => state.size(),
            UpdateData::Delta(delta) => delta.size(),
            UpdateData::StateAndDelta { state, delta } => state.size() + delta.size(),
            UpdateData::RelatedState { state, .. } => state.size() + CONTRACT_KEY_SIZE,
            UpdateData::RelatedDelta { delta, .. } => delta.size() + CONTRACT_KEY_SIZE,
            UpdateData::RelatedStateAndDelta { state, delta, .. } => {
                state.size() + delta.size() + CONTRACT_KEY_SIZE
            }
        }
    }

    pub fn unwrap_delta(&self) -> &StateDelta<'_> {
        match self {
            UpdateData::Delta(delta) => delta,
            _ => panic!(),
        }
    }

    /// Copies the data if not owned and returns an owned version of self.
    pub fn into_owned(self) -> UpdateData<'static> {
        match self {
            UpdateData::State(s) => UpdateData::State(State::from(s.into_bytes())),
            UpdateData::Delta(d) => UpdateData::Delta(StateDelta::from(d.into_bytes())),
            UpdateData::StateAndDelta { state, delta } => UpdateData::StateAndDelta {
                delta: StateDelta::from(delta.into_bytes()),
                state: State::from(state.into_bytes()),
            },
            UpdateData::RelatedState { related_to, state } => UpdateData::RelatedState {
                related_to,
                state: State::from(state.into_bytes()),
            },
            UpdateData::RelatedDelta { related_to, delta } => UpdateData::RelatedDelta {
                related_to,
                delta: StateDelta::from(delta.into_bytes()),
            },
            UpdateData::RelatedStateAndDelta {
                related_to,
                state,
                delta,
            } => UpdateData::RelatedStateAndDelta {
                related_to,
                state: State::from(state.into_bytes()),
                delta: StateDelta::from(delta.into_bytes()),
            },
        }
    }

    pub(crate) fn get_self_states<'a>(
        updates: &[UpdateData<'a>],
    ) -> Vec<(Option<State<'a>>, Option<StateDelta<'a>>)> {
        let mut own_states = Vec::with_capacity(updates.len());
        for update in updates {
            match update {
                UpdateData::State(state) => own_states.push((Some(state.clone()), None)),
                UpdateData::Delta(delta) => own_states.push((None, Some(delta.clone()))),
                UpdateData::StateAndDelta { state, delta } => {
                    own_states.push((Some(state.clone()), Some(delta.clone())))
                }
                _ => {}
            }
        }
        own_states
    }

    pub(crate) fn deser_update_data<'de, D>(deser: D) -> Result<UpdateData<'static>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <UpdateData as Deserialize>::deserialize(deser)?;
        Ok(value.into_owned())
    }
}

impl<'a> From<StateDelta<'a>> for UpdateData<'a> {
    fn from(delta: StateDelta<'a>) -> Self {
        UpdateData::Delta(delta)
    }
}

impl<'a> TryFromFbs<&FbsUpdateData<'a>> for UpdateData<'a> {
    fn try_decode_fbs(update_data: &FbsUpdateData<'a>) -> Result<Self, WsApiError> {
        match update_data.update_data_type() {
            UpdateDataType::StateUpdate => {
                let update = update_data.update_data_as_state_update().unwrap();
                let state = State::from(update.state().bytes());
                Ok(UpdateData::State(state))
            }
            UpdateDataType::DeltaUpdate => {
                let update = update_data.update_data_as_delta_update().unwrap();
                let delta = StateDelta::from(update.delta().bytes());
                Ok(UpdateData::Delta(delta))
            }
            UpdateDataType::StateAndDeltaUpdate => {
                let update = update_data.update_data_as_state_and_delta_update().unwrap();
                let state = State::from(update.state().bytes());
                let delta = StateDelta::from(update.delta().bytes());
                Ok(UpdateData::StateAndDelta { state, delta })
            }
            UpdateDataType::RelatedStateUpdate => {
                let update = update_data.update_data_as_related_state_update().unwrap();
                let state = State::from(update.state().bytes());
                let related_to =
                    ContractInstanceId::from_bytes(update.related_to().data().bytes()).unwrap();
                Ok(UpdateData::RelatedState { related_to, state })
            }
            UpdateDataType::RelatedDeltaUpdate => {
                let update = update_data.update_data_as_related_delta_update().unwrap();
                let delta = StateDelta::from(update.delta().bytes());
                let related_to =
                    ContractInstanceId::from_bytes(update.related_to().data().bytes()).unwrap();
                Ok(UpdateData::RelatedDelta { related_to, delta })
            }
            UpdateDataType::RelatedStateAndDeltaUpdate => {
                let update = update_data
                    .update_data_as_related_state_and_delta_update()
                    .unwrap();
                let state = State::from(update.state().bytes());
                let delta = StateDelta::from(update.delta().bytes());
                let related_to =
                    ContractInstanceId::from_bytes(update.related_to().data().bytes()).unwrap();
                Ok(UpdateData::RelatedStateAndDelta {
                    related_to,
                    state,
                    delta,
                })
            }
            _ => unreachable!(),
        }
    }
}
```

4. **Error Handling Strategy**
- What should happen when synchronization fails?
- Should we retry failed operations? If so, with what backoff strategy?

5. **Initialization Flow**
- When should the initial synchronization happen?
- How do we handle the initial state merge between local and remote?


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

```rust
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContractRequest<'a> {
    /// Insert a new value in a contract corresponding with the provided key.
    Put {
        contract: ContractContainer,
        /// Value to upsert in the contract.
        state: WrappedState,
        /// Related contracts.
        #[serde(borrow)]
        related_contracts: RelatedContracts<'a>,
    },
    /// Update an existing contract corresponding with the provided key.
    Update {
        key: ContractKey,
        #[serde(borrow)]
        data: UpdateData<'a>,
    },
    /// Fetch the current state from a contract corresponding to the provided key.
    Get {
        /// Key of the contract.
        key: ContractKey,
        /// If this flag is set then fetch also the contract itself.
        return_contract_code: bool,
    },
    /// Subscribe to the changes in a given contract. Implicitly starts a get operation
    /// if the contract is not present yet.
    Subscribe {
        key: ContractKey,
        summary: Option<StateSummary<'a>>,
    },
}
```

# Contract keys and related functions
```rust
/// A complete key specification, that represents a cryptographic hash that identifies the contract.
#[serde_as]
#[derive(Debug, Eq, Copy, Clone, Serialize, Deserialize)]
#[cfg_attr(any(feature = "testing", test), derive(arbitrary::Arbitrary))]
pub struct ContractKey {
    instance: ContractInstanceId,
    code: Option<CodeHash>,
}

impl ContractKey {
    pub fn from_params_and_code<'a>(
        params: impl Borrow<Parameters<'a>>,
        wasm_code: impl Borrow<ContractCode<'a>>,
    ) -> Self {
        let code = wasm_code.borrow();
        let id = generate_id(params.borrow(), code);
        let code_hash = code.hash();
        Self {
            instance: id,
            code: Some(*code_hash),
        }
    }

    /// Builds a partial [`ContractKey`](ContractKey), the contract code part is unspecified.
    pub fn from_id(instance: impl Into<String>) -> Result<Self, bs58::decode::Error> {
        let instance = ContractInstanceId::try_from(instance.into())?;
        Ok(Self {
            instance,
            code: None,
        })
    }

    /// Gets the whole spec key hash.
    pub fn as_bytes(&self) -> &[u8] {
        self.instance.0.as_ref()
    }

    /// Returns the hash of the contract code only, if the key is fully specified.
    pub fn code_hash(&self) -> Option<&CodeHash> {
        self.code.as_ref()
    }

    /// Returns the encoded hash of the contract code, if the key is fully specified.
    pub fn encoded_code_hash(&self) -> Option<String> {
        self.code.as_ref().map(|c| {
            bs58::encode(c.0)
                .with_alphabet(bs58::Alphabet::BITCOIN)
                .into_string()
        })
    }

    /// Returns the contract key from the encoded hash of the contract code and the given
    /// parameters.
    pub fn from_params(
        code_hash: impl Into<String>,
        parameters: Parameters,
    ) -> Result<Self, bs58::decode::Error> {
        let mut code_key = [0; CONTRACT_KEY_SIZE];
        bs58::decode(code_hash.into())
            .with_alphabet(bs58::Alphabet::BITCOIN)
            .onto(&mut code_key)?;

        let mut hasher = Blake3::new();
        hasher.update(code_key.as_slice());
        hasher.update(parameters.as_ref());
        let full_key_arr = hasher.finalize();

        let mut spec = [0; CONTRACT_KEY_SIZE];
        spec.copy_from_slice(&full_key_arr);
        Ok(Self {
            instance: ContractInstanceId(spec),
            code: Some(CodeHash(code_key)),
        })
    }

    /// Returns the `Base58` encoded string of the [`ContractInstanceId`](ContractInstanceId).
    pub fn encoded_contract_id(&self) -> String {
        self.instance.encode()
    }

    pub fn id(&self) -> &ContractInstanceId {
        &self.instance
    }
}

impl PartialEq for ContractKey {
    fn eq(&self, other: &Self) -> bool {
        self.instance == other.instance
    }
}

impl std::hash::Hash for ContractKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.instance.0.hash(state);
    }
}

impl From<ContractInstanceId> for ContractKey {
    fn from(instance: ContractInstanceId) -> Self {
        Self {
            instance,
            code: None,
        }
    }
}

impl From<ContractKey> for ContractInstanceId {
    fn from(key: ContractKey) -> Self {
        key.instance
    }
}

```

Here is documentation on how to fetch data from an API within Dioxus, this may indicate
how to update the Freenet node when the Rooms signal changes, although we'll need to
be careful to avoid infinite loops.

```rust