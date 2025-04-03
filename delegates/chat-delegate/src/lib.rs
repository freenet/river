use freenet_stdlib::{
    prelude::{
        delegate, ApplicationMessage, DelegateContext, DelegateError, DelegateInterface,
        GetSecretRequest, InboundDelegateMsg, OutboundDelegateMsg, Parameters, SecretsId,
        SetSecretRequest,
    },
};

// Custom logging module to handle different environments
mod logging {
    #[cfg(target_arch = "wasm32")]
    pub fn info(msg: &str) {
        freenet_stdlib::log::info(msg);
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn info(msg: &str) {
        println!("[INFO] {}", msg);
    }
}
use river_common::chat_delegate::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use serde::ser::SerializeTuple;

// Constants
const KEY_INDEX_SUFFIX: &str = "::key_index";
const ORIGIN_KEY_SEPARATOR: &str = ":";

/// Different types of pending operations
#[derive(Debug, Clone)]
enum PendingOperation {
    /// Regular get operation for a specific key
    Get {
        origin: Origin,
        client_key: ChatDelegateKey,
    },
    /// Store operation that needs to update the index
    Store {
        origin: Origin,
        client_key: ChatDelegateKey,
    },
    /// Delete operation that needs to update the index
    Delete {
        origin: Origin,
        client_key: ChatDelegateKey,
    },
    /// List operation to retrieve all keys
    List {
        origin: Origin,
    },
}

impl PendingOperation {
    fn is_delete_operation(&self) -> bool {
        matches!(self, Self::Delete { .. })
    }
}

/// Chat delegate for storing and retrieving data in the Freenet secret storage.
///
/// This delegate provides a key-value store interface for chat applications,
/// maintaining an index of keys for each application and handling storage,
/// retrieval, deletion, and listing operations.
pub struct ChatDelegate;

/// Parameters for the chat delegate.
/// Currently empty, but could be extended with configuration options.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatDelegateParameters;

impl TryFrom<Parameters<'_>> for ChatDelegateParameters {
    type Error = DelegateError;

    fn try_from(_params: Parameters<'_>) -> Result<Self, Self::Error> {
        // Currently no parameters are used, but this could be extended
        Ok(Self {})
    }
}

#[delegate]
impl DelegateInterface for ChatDelegate {
    fn process(
        _parameters: Parameters<'static>,
        attested: Option<&'static [u8]>,
        message: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        let message_type = match message {
            InboundDelegateMsg::ApplicationMessage(_) => "application message",
            InboundDelegateMsg::GetSecretResponse(_) => "get secret response",
            InboundDelegateMsg::UserResponse(_) => "user response",
            InboundDelegateMsg::GetSecretRequest(_) => "get secret request",
        };

        logging::info(
            format!("Delegate received ApplicationMessage of type {message_type}").as_str(),
        );

        // Verify that attested is provided - this is the authenticated origin
        let origin: Origin = match attested {
            Some(origin) => Origin(origin.to_vec()),
            None => {
                return Err(DelegateError::Other(format!(
                    "missing attested origin for message type: {:?}",
                    message_type
                )))
            }
        };

        match message {
            InboundDelegateMsg::ApplicationMessage(app_msg) => {
                if app_msg.processed {
                    return Err(DelegateError::Other(
                        "cannot process an already processed message".into(),
                    ));
                }

                handle_application_message(app_msg, &origin)
            }
            InboundDelegateMsg::GetSecretResponse(response) => handle_get_secret_response(response),
            InboundDelegateMsg::UserResponse(_) => {
                // We don't use user responses in this delegate
                Ok(vec![])
            }
            InboundDelegateMsg::GetSecretRequest(_) => {
                // We don't handle direct get secret requests
                Err(DelegateError::Other(
                    "unexpected message type: get secret request".into(),
                ))
            }
        }
    }
}

/// A wrapper around SecretsId that implements Hash and Eq
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
struct SecretIdKey(String);

impl From<&SecretsId> for SecretIdKey {
    fn from(id: &SecretsId) -> Self {
        // Convert the SecretsId to a string representation for hashing
        Self(String::from_utf8_lossy(id.key()).to_string())
    }
}

/// Context for the chat delegate, storing pending operations.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ChatDelegateContext {
    /// Map of secret IDs to pending operations
    pending_ops: HashMap<SecretIdKey, PendingOperation>,
}

/// Structure to store the index of keys for an app
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct KeyIndex {
    keys: Vec<ChatDelegateKey>,
}

impl TryFrom<DelegateContext> for ChatDelegateContext {
    type Error = DelegateError;

    fn try_from(value: DelegateContext) -> Result<Self, Self::Error> {
        if value == DelegateContext::default() {
            return Ok(Self::default());
        }
        ciborium::from_reader(value.as_ref())
            .map_err(|err| DelegateError::Deser(format!("Failed to deserialize context: {err}")))
    }
}

impl TryFrom<&ChatDelegateContext> for DelegateContext {
    type Error = DelegateError;

    fn try_from(value: &ChatDelegateContext) -> Result<Self, Self::Error> {
        let mut buffer = Vec::new();
        ciborium::ser::into_writer(value, &mut buffer)
            .map_err(|err| DelegateError::Deser(format!("Failed to serialize context: {err}")))?;
        Ok(DelegateContext::new(buffer))
    }
}

impl TryFrom<&mut ChatDelegateContext> for DelegateContext {
    type Error = DelegateError;

    fn try_from(value: &mut ChatDelegateContext) -> Result<Self, Self::Error> {
        // Delegate to the immutable reference implementation
        Self::try_from(&*value)
    }
}

// Add Serialize/Deserialize for PendingOperation
impl Serialize for PendingOperation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Get { origin, client_key } => {
                let mut seq = serializer.serialize_tuple(3)?;
                seq.serialize_element(&0u8)?; // Type tag for Get
                seq.serialize_element(origin)?;
                seq.serialize_element(client_key)?;
                seq.end()
            }
            Self::Store { origin, client_key } => {
                let mut seq = serializer.serialize_tuple(3)?;
                seq.serialize_element(&1u8)?; // Type tag for Store
                seq.serialize_element(origin)?;
                seq.serialize_element(client_key)?;
                seq.end()
            }
            Self::Delete { origin, client_key } => {
                let mut seq = serializer.serialize_tuple(3)?;
                seq.serialize_element(&2u8)?; // Type tag for Delete
                seq.serialize_element(origin)?;
                seq.serialize_element(client_key)?;
                seq.end()
            }
            Self::List { origin } => {
                let mut seq = serializer.serialize_tuple(2)?;
                seq.serialize_element(&3u8)?; // Type tag for List
                seq.serialize_element(origin)?;
                seq.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for PendingOperation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error, SeqAccess, Visitor};
        use std::fmt;

        struct PendingOpVisitor;

        impl<'de> Visitor<'de> for PendingOpVisitor {
            type Value = PendingOperation;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a tuple with a type tag and operation data")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let tag: u8 = seq.next_element()?.ok_or_else(|| Error::invalid_length(0, &self))?;
                
                match tag {
                    0 => { // Get
                        let origin: Origin = seq.next_element()?.ok_or_else(|| Error::invalid_length(1, &self))?;
                        let client_key: ChatDelegateKey = seq.next_element()?.ok_or_else(|| Error::invalid_length(2, &self))?;
                        Ok(PendingOperation::Get { origin, client_key })
                    },
                    1 => { // Store
                        let origin: Origin = seq.next_element()?.ok_or_else(|| Error::invalid_length(1, &self))?;
                        let client_key: ChatDelegateKey = seq.next_element()?.ok_or_else(|| Error::invalid_length(2, &self))?;
                        Ok(PendingOperation::Store { origin, client_key })
                    },
                    2 => { // Delete
                        let origin: Origin = seq.next_element()?.ok_or_else(|| Error::invalid_length(1, &self))?;
                        let client_key: ChatDelegateKey = seq.next_element()?.ok_or_else(|| Error::invalid_length(2, &self))?;
                        Ok(PendingOperation::Delete { origin, client_key })
                    },
                    3 => { // List
                        let origin: Origin = seq.next_element()?.ok_or_else(|| Error::invalid_length(1, &self))?;
                        Ok(PendingOperation::List { origin })
                    },
                    _ => Err(Error::custom(format!("Unknown operation type tag: {}", tag))),
                }
            }
        }

        deserializer.deserialize_tuple(3, PendingOpVisitor)
    }
}

/// Handle an application message
fn handle_application_message(
    app_msg: ApplicationMessage,
    origin: &Origin,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let mut context = ChatDelegateContext::try_from(app_msg.context)?;

    // Deserialize the request message
    let request: ChatDelegateRequestMsg = ciborium::from_reader(app_msg.payload.as_slice())
        .map_err(|e| DelegateError::Deser(format!("Failed to deserialize request: {e}")))?;

    match request {
        ChatDelegateRequestMsg::StoreRequest { key, value } => {
            logging::info(
                format!("Delegate received StoreRequest key: {key:?}, value: {value:?}").as_str(),
            );
            handle_store_request(&mut context, origin, key, value)
        }
        ChatDelegateRequestMsg::GetRequest { key } => {
            logging::info(
                format!("Delegate received GetRequest key: {key:?}").as_str(),
            );
            handle_get_request(&mut context, origin, key)
        }
        ChatDelegateRequestMsg::DeleteRequest { key } => {
            logging::info(
                format!("Delegate received DeleteRequest key: {key:?}").as_str(),
            );
            handle_delete_request(&mut context, origin, key)
        }
        ChatDelegateRequestMsg::ListRequest => {
            logging::info("Delegate received ListRequest");
            handle_list_request(&mut context, origin)
        }
    }
}

/// Handle a store request
fn handle_store_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    key: ChatDelegateKey,
    value: Vec<u8>,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this app's data
    let secret_id = create_origin_key(origin, &key);

    // Create the index key
    let index_key = create_index_key(origin);

    // Store the original request in context for later processing after we get the index
    context
        .pending_ops
        .insert(SecretIdKey::from(&index_key), PendingOperation::Store {
            origin: origin.clone(),
            client_key: key.clone(),
        });

    // Create response for the client
    let response = ChatDelegateResponseMsg::StoreResponse {
        key: key.clone(),
        result: Ok(()),
        value_size: value.len(),
    };

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create the three messages we need to send:
    // 1. Response to the client
    let app_response = create_app_response(&response, &context_bytes)?;

    // 2. Store the actual value
    let set_secret = OutboundDelegateMsg::SetSecretRequest(SetSecretRequest {
        key: secret_id,
        value: Some(value),
    });

    // 3. Request the current index to update it
    let get_index = create_get_index_request(index_key, &context_bytes)?;

    // Return all messages
    Ok(vec![app_response, set_secret, get_index])
}

/// Handle a get request
fn handle_get_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    key: ChatDelegateKey,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this origin contract's data
    let secret_id = create_origin_key(origin, &key);

    // Store the original request in context for later processing
    context
        .pending_ops
        .insert(SecretIdKey::from(&secret_id), PendingOperation::Get {
            origin: origin.clone(),
            client_key: key.clone(),
        });

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create and return the get request
    let get_secret = create_get_request(secret_id, &context_bytes)?;

    Ok(vec![get_secret])
}

/// Handle a delete request
fn handle_delete_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    key: ChatDelegateKey,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this app's data
    let secret_id = create_origin_key(origin, &key);

    // Create the index key
    let index_key = create_index_key(origin);

    // Store the original request in context for later processing after we get the index
    context
        .pending_ops
        .insert(SecretIdKey::from(&index_key), PendingOperation::Delete {
            origin: origin.clone(),
            client_key: key.clone(),
        });

    // Create response for the client
    let response = ChatDelegateResponseMsg::DeleteResponse {
        key: key.clone(),
        result: Ok(()),
    };

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create the three messages we need to send:
    // 1. Response to the client
    let app_response = create_app_response(&response, &context_bytes)?;

    // 2. Delete the actual value
    let set_secret = OutboundDelegateMsg::SetSecretRequest(SetSecretRequest {
        key: secret_id,
        value: None, // Setting to None deletes the secret
    });

    // 3. Request the current index to update it
    let get_index = create_get_index_request(index_key, &context_bytes)?;

    // Return all messages
    Ok(vec![app_response, set_secret, get_index])
}

/// Handle a list request
fn handle_list_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create the index key
    let index_key = create_index_key(origin);

    // Store a special marker in the context to indicate this is a list request
    context
        .pending_ops
        .insert(SecretIdKey::from(&index_key), PendingOperation::List {
            origin: origin.clone(),
        });

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create and return the get index request
    let get_index = create_get_index_request(index_key, &context_bytes)?;

    Ok(vec![get_index])
}

/// Handle a get secret response
fn handle_get_secret_response(
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    logging::info("Received GetSecretResponse");

    // Deserialize context
    let mut context = ChatDelegateContext::try_from(get_secret_response.context.clone())?;

    // Get the key as a string to check if it's an index key
    let key_str = String::from_utf8_lossy(get_secret_response.key.key()).to_string();
    let key_clone = get_secret_response.key.clone();

    // Check if this is a key index response
    if key_str.ends_with(KEY_INDEX_SUFFIX) {
        handle_key_index_response(&key_clone, &mut context, get_secret_response)
    } else {
        handle_regular_get_response(&key_clone, &mut context, get_secret_response)
    }
}

/// Helper function to create a unique app key
fn create_origin_key(origin: &Origin, key: &ChatDelegateKey) -> SecretsId {
    SecretsId::new(
        format!("{}{}{}", origin.to_b58(), ORIGIN_KEY_SEPARATOR, String::from_utf8_lossy(key.as_bytes()).to_string()).into_bytes()
    )
}

/// Helper function to create an index key
fn create_index_key(origin: &Origin) -> SecretsId {
    SecretsId::new(format!(
        "{}{}{}",
        origin.to_b58(),
        ORIGIN_KEY_SEPARATOR,
        KEY_INDEX_SUFFIX
    ).into_bytes())
}

/// Helper function to create a get request
fn create_get_request(
    secret_id: SecretsId,
    context: &DelegateContext,
) -> Result<OutboundDelegateMsg, DelegateError> {
    let get_secret = OutboundDelegateMsg::GetSecretRequest(GetSecretRequest {
        key: secret_id,
        context: context.clone(),
        processed: false,
    });

    Ok(get_secret)
}

/// Helper function to create a get index request
fn create_get_index_request(
    index_secret_id: SecretsId,
    context: &DelegateContext,
) -> Result<OutboundDelegateMsg, DelegateError> {
    let get_index = OutboundDelegateMsg::GetSecretRequest(GetSecretRequest {
        key: index_secret_id,
        context: context.clone(),
        processed: false,
    });

    Ok(get_index)
}

/// Helper function to create an app response
fn create_app_response<T: Serialize>(
    response: &T,
    context: &DelegateContext,
) -> Result<OutboundDelegateMsg, DelegateError> {
    // Serialize response
    let mut response_bytes = Vec::new();
    ciborium::ser::into_writer(response, &mut response_bytes)
        .map_err(|e| DelegateError::Deser(format!("Failed to serialize response: {e}")))?;

    // Create response message
    Ok(OutboundDelegateMsg::ApplicationMessage(
        ApplicationMessage::new(response_bytes)
            .with_context(context.clone())
            .processed(true),
    ))
}

/// Handle a key index response
fn handle_key_index_response(
    secret_id: &SecretsId,
    context: &mut ChatDelegateContext,
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    logging::info("Handling key index response");
    
    // This is a response to a key index request
    let secret_id_key = SecretIdKey::from(secret_id);
    if let Some(pending_op) = context.pending_ops.get(&secret_id_key).cloned() {
        let mut outbound_msgs = Vec::new();

        // Parse the key index or create a new one if it doesn't exist
        let mut key_index = if let Some(index_data) = &get_secret_response.value {
            ciborium::from_reader(index_data.as_slice()).map_err(|e| {
                DelegateError::Deser(format!("Failed to deserialize key index: {e}"))
            })?
        } else {
            KeyIndex::default()
        };

        match &pending_op {
            PendingOperation::List { .. } => {
                // Create list response
                let response = ChatDelegateResponseMsg::ListResponse {
                    keys: key_index.keys.clone(),
                };

                // Create response message
                let context_bytes = DelegateContext::try_from(&ChatDelegateContext::default())?;
                let app_response = create_app_response(&response, &context_bytes)?;
                outbound_msgs.push(app_response);
            },
            PendingOperation::Store { client_key, .. } | PendingOperation::Delete { client_key, .. } => {
                // This is a store or delete operation that needs to update the index
                let is_delete = pending_op.is_delete_operation();

                if is_delete {
                    // For delete operations, remove the key
                    key_index.keys.retain(|k| k != client_key);
                } else {
                    // For store operations, add the key if it doesn't exist
                    if !key_index.keys.contains(client_key) {
                        key_index.keys.push(client_key.clone());
                    }
                }

                // Serialize the updated index
                let mut index_bytes = Vec::new();
                ciborium::ser::into_writer(&key_index, &mut index_bytes)
                    .map_err(|e| DelegateError::Deser(format!("Failed to serialize key index: {e}")))?;

                // Create set secret request to update the index
                let set_index = OutboundDelegateMsg::SetSecretRequest(SetSecretRequest {
                    key: secret_id.clone(),
                    value: Some(index_bytes),
                });

                outbound_msgs.push(set_index);
            },
            PendingOperation::Get { .. } => {
                return Err(DelegateError::Other(
                    "Unexpected Get operation for key index response".to_string()
                ));
            }
        }

        // Remove the pending operation
        context.pending_ops.remove(&secret_id_key);

        Ok(outbound_msgs)
    } else {
        // No pending operation for this key index
        Err(DelegateError::Other(format!(
            "No pending key index request for: {secret_id:?}"
        )))
    }
}

/// Handle a regular get response
fn handle_regular_get_response(
    secret_id: &SecretsId,
    context: &mut ChatDelegateContext,
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    logging::info("Handling regular get response");
    
    let secret_id_key = SecretIdKey::from(secret_id);
    if let Some(PendingOperation::Get { client_key, .. }) = context.pending_ops.get(&secret_id_key).cloned() {
        // Create response
        let response = ChatDelegateResponseMsg::GetResponse {
            key: client_key,
            value: get_secret_response.value,
        };

        // Create response message
        let context_bytes = DelegateContext::try_from(&ChatDelegateContext::default())?;
        let app_response = create_app_response(&response, &context_bytes)?;

        // Remove the pending get request
        context.pending_ops.remove(&secret_id_key);

        Ok(vec![app_response])
    } else {
        let key_str = String::from_utf8_lossy(secret_id.key()).to_string();
        logging::info(format!("No pending get request for key: {key_str}").as_str());
        Err(DelegateError::Other(format!(
            "No pending get request for key: {key_str}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use freenet_stdlib::prelude::DelegateContext;

    /// Helper function to create empty parameters for testing
    fn create_test_parameters() -> Parameters<'static> {
        Parameters::from(vec![])
    }

    /// Helper function to create an application message
    fn create_app_message(request: ChatDelegateRequestMsg) -> ApplicationMessage {
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&request, &mut payload)
            .map_err(|e| panic!("Failed to serialize request: {e}"))
            .unwrap();
        ApplicationMessage::new(payload)
    }

    /// Helper function to extract response from outbound messages
    fn extract_response(messages: Vec<OutboundDelegateMsg>) -> Option<ChatDelegateResponseMsg> {
        for msg in messages {
            if let OutboundDelegateMsg::ApplicationMessage(app_msg) = msg {
                return ciborium::from_reader(app_msg.payload.as_slice())
                    .map_err(|e| panic!("Failed to deserialize response: {e}"))
                    .ok();
            }
        }
        None
    }

    #[test]
    fn test_store_request() {
        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        let request = ChatDelegateRequestMsg::StoreRequest {
            key: river_common::chat_delegate::ChatDelegateKey(key.clone()),
            value: value.clone(),
        };

        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 3 messages: app response, set secret, get index
        assert_eq!(result.len(), 3);

        // Check app response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::StoreResponse {
                key: resp_key,
                result,
                value_size,
            } => {
                assert_eq!(resp_key, river_common::chat_delegate::ChatDelegateKey(key.clone()));
                assert!(result.is_ok());
                assert_eq!(value_size, value.len());
            }
            _ => panic!("Expected StoreResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_request() {
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::GetRequest { key: river_common::chat_delegate::ChatDelegateKey(key.clone()) };
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: get secret request
        assert_eq!(result.len(), 1);

        // Check it's a get secret request
        match &result[0] {
            OutboundDelegateMsg::GetSecretRequest(req) => {
                // Verify the key contains our app ID and key
                let key_str = String::from_utf8(req.key.key().to_vec())
                    .map_err(|e| panic!("Invalid UTF-8 in key: {e}"))
                    .unwrap();
                assert!(key_str.contains(&bs58::encode(&key).into_string()));
            }
            _ => panic!("Expected GetSecretRequest, got {:?}", result[0]),
        }
    }

    #[test]
    fn test_delete_request() {
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::DeleteRequest { key: river_common::chat_delegate::ChatDelegateKey(key.clone()) };
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 3 messages: app response, set secret (with None value), get index
        assert_eq!(result.len(), 3);

        // Check app response
        let response = extract_response(result.clone()).unwrap();
        match response {
            ChatDelegateResponseMsg::DeleteResponse {
                key: resp_key,
                result,
            } => {
                assert_eq!(resp_key, river_common::chat_delegate::ChatDelegateKey(key.clone()));
                assert!(result.is_ok());
            }
            _ => panic!("Expected DeleteResponse, got {:?}", response),
        }

        // Check set secret request has None value (deletion)
        let mut found_set_request = false;
        for msg in result {
            if let OutboundDelegateMsg::SetSecretRequest(req) = msg {
                if req.value.is_none() {
                    found_set_request = true;
                    break;
                }
            }
        }
        assert!(
            found_set_request,
            "No SetSecretRequest with None value found"
        );
    }

    #[test]
    fn test_list_request() {
        let request = ChatDelegateRequestMsg::ListRequest;
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: get index request
        assert_eq!(result.len(), 1);

        // Check it's a get secret request for the index
        match &result[0] {
            OutboundDelegateMsg::GetSecretRequest(req) => {
                // Verify the key contains our app ID and key_index suffix
                let key_str = String::from_utf8(req.key.key().to_vec())
                    .map_err(|e| panic!("Invalid UTF-8 in key: {e}"))
                    .unwrap();
                assert!(key_str.contains(KEY_INDEX_SUFFIX));
            }
            _ => panic!("Expected GetSecretRequest, got {:?}", result[0]),
        }
    }

    #[test]
    fn test_get_secret_response_for_regular_get() {
        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        // Create a context with a pending get
        let mut context = ChatDelegateContext::default();

        let test_origin = create_test_origin();

        let key_delegate = river_common::chat_delegate::ChatDelegateKey(key.clone());
        let app_key = create_origin_key(&test_origin, &key_delegate);
        context
            .pending_ops
            .insert(SecretIdKey::from(&app_key), PendingOperation::Get {
                origin: test_origin.clone(),
                client_key: river_common::chat_delegate::ChatDelegateKey(key.clone()),
            });

        // Serialize the context
        let context_bytes = DelegateContext::try_from(&context)
            .map_err(|e| panic!("Failed to serialize context: {e}"))
            .unwrap();

        // Create a get secret response
        let get_response = freenet_stdlib::prelude::GetSecretResponse {
            key: app_key.clone(),
            value: Some(value.clone()),
            context: context_bytes,
        };

        let inbound_msg = InboundDelegateMsg::GetSecretResponse(get_response);

        // Pass the attested origin parameter
        let result = ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check app response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::GetResponse {
                key: resp_key,
                value: resp_value,
            } => {
                assert_eq!(resp_key, river_common::chat_delegate::ChatDelegateKey(key.clone()));
                assert_eq!(resp_value, Some(value));
            }
            _ => panic!("Expected GetResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_secret_response_for_list_request() {
        let keys = vec![b"key1".to_vec(), b"key2".to_vec(), b"key3".to_vec()];

        // Create a key index with some keys
        let wrapped_keys: Vec<river_common::chat_delegate::ChatDelegateKey> = keys.clone()
            .into_iter()
            .map(|k| river_common::chat_delegate::ChatDelegateKey(k))
            .collect();
        let key_index = KeyIndex { keys: wrapped_keys };
        let mut index_bytes = Vec::new();
        ciborium::ser::into_writer(&key_index, &mut index_bytes)
            .map_err(|e| panic!("Failed to serialize key index: {e}"))
            .unwrap();

        let test_origin = create_test_origin();

        // Create a context with a pending list request
        let mut context = ChatDelegateContext::default();
        let index_key = create_index_key(&test_origin);
        context
            .pending_ops
            .insert(SecretIdKey::from(&index_key), PendingOperation::List {
                origin: test_origin.clone(),
            });

        // Serialize the context
        let context_bytes = DelegateContext::try_from(&context)
            .map_err(|e| panic!("Failed to serialize context: {e}"))
            .unwrap();

        // Create a get secret response for the index
        let get_response = freenet_stdlib::prelude::GetSecretResponse {
            key: index_key.clone(),
            value: Some(index_bytes),
            context: context_bytes,
        };

        let inbound_msg = InboundDelegateMsg::GetSecretResponse(get_response);

        // Pass the attested origin parameter
        let result = ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check app response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::ListResponse { keys: resp_keys } => {
                let wrapped_keys: Vec<river_common::chat_delegate::ChatDelegateKey> = keys.clone()
                    .into_iter()
                    .map(|k| river_common::chat_delegate::ChatDelegateKey(k))
                    .collect();
                assert_eq!(resp_keys, wrapped_keys);
            }
            _ => panic!("Expected ListResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_secret_response_for_store_request() {
        let key = b"test_key".to_vec();

        // Create a key index with some existing keys
        let existing_keys = vec![b"existing_key".to_vec()];
        let key_index = KeyIndex {
            keys: existing_keys.into_iter()
                .map(|k| river_common::chat_delegate::ChatDelegateKey(k))
                .collect(),
        };
        let mut index_bytes = Vec::new();
        ciborium::ser::into_writer(&key_index, &mut index_bytes)
            .map_err(|e| panic!("Failed to serialize key index: {e}"))
            .unwrap();

        let test_origin = create_test_origin();

        // Create a context with a pending store request
        let mut context = ChatDelegateContext::default();
        let index_key = create_index_key(&test_origin);
        context
            .pending_ops
            .insert(SecretIdKey::from(&index_key), PendingOperation::Store {
                origin: test_origin.clone(),
                client_key: river_common::chat_delegate::ChatDelegateKey(key.clone()),
            });

        // Serialize the context
        let context_bytes = DelegateContext::try_from(&context)
            .map_err(|e| panic!("Failed to serialize context: {e}"))
            .unwrap();

        // Create a get secret response for the index
        let get_response = freenet_stdlib::prelude::GetSecretResponse {
            key: index_key.clone(),
            value: Some(index_bytes),
            context: context_bytes,
        };

        let inbound_msg = InboundDelegateMsg::GetSecretResponse(get_response);

        // Pass the attested origin parameter
        let result = ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        )
        .unwrap();

        // Should have 1 message: set secret request to update the index
        assert_eq!(result.len(), 1);

        // Check it's a set secret request
        match &result[0] {
            OutboundDelegateMsg::SetSecretRequest(req) => {
                // Deserialize the value to check the updated index
                let updated_index: KeyIndex =
                    ciborium::from_reader(req.value.as_ref().unwrap().as_slice())
                        .map_err(|e| panic!("Failed to deserialize updated index: {e}"))
                        .unwrap();

                // Should contain both the existing key and our new key
                assert_eq!(updated_index.keys.len(), 2);
                let key_wrapped = river_common::chat_delegate::ChatDelegateKey(key.clone());
                let existing_key_wrapped = river_common::chat_delegate::ChatDelegateKey(b"existing_key".to_vec());
                assert!(updated_index.keys.contains(&key_wrapped));
                assert!(updated_index.keys.contains(&existing_key_wrapped));
            }
            _ => panic!("Expected SetSecretRequest, got {:?}", result[0]),
        }
    }

    #[test]
    fn test_error_on_processed_message() {
        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        let request = ChatDelegateRequestMsg::StoreRequest { 
            key: river_common::chat_delegate::ChatDelegateKey(key), 
            value 
        };

        let mut app_msg = create_app_message(request);
        app_msg = app_msg.processed(true); // Mark as already processed
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let _origin = create_test_origin();

        let result = ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        );
        assert!(result.is_err());

        if let Err(DelegateError::Other(msg)) = result {
            assert!(msg.contains("cannot process an already processed message"));
        } else {
            panic!("Expected DelegateError::Other, got {:?}", result);
        }
    }

    #[test]
    fn test_error_on_unexpected_message_type() {
        let get_secret_request = GetSecretRequest {
            key: SecretsId::new(vec![1, 2, 3]),
            context: DelegateContext::default(),
            processed: false,
        };

        let inbound_msg = InboundDelegateMsg::GetSecretRequest(get_secret_request);

        let _origin = create_test_origin();

        let result = ChatDelegate::process(
            create_test_parameters(),
            Some(get_test_origin_bytes()),
            inbound_msg,
        );
        assert!(result.is_err());

        if let Err(DelegateError::Other(msg)) = result {
            assert!(msg.contains("unexpected message type"));
        } else {
            panic!("Expected DelegateError::Other, got {:?}", result);
        }
    }

    #[test]
    fn test_error_on_missing_attested() {
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::GetRequest { 
            key: river_common::chat_delegate::ChatDelegateKey(key) 
        };
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        // Pass None for attested
        let result = ChatDelegate::process(create_test_parameters(), None, inbound_msg);
        assert!(result.is_err());

        if let Err(DelegateError::Other(msg)) = result {
            assert!(msg.contains("missing attested origin"));
        } else {
            panic!("Expected DelegateError::Other, got {:?}", result);
        }
    }

    // Helper function to create a test origin
    fn create_test_origin() -> Origin {
        Origin(vec![0u8, 0u8, 0u8, 0u8])
    }

    // Get the bytes from the test origin for process calls
    fn get_test_origin_bytes() -> &'static [u8] {
        // Using lazy_static would be better in a real app
        // but for tests this is simpler
        static TEST_ORIGIN: [u8; 4] = [0u8, 0u8, 0u8, 0u8];
        &TEST_ORIGIN
    }
}

/// Origin contract ID
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Origin(Vec<u8>);

impl Origin {
    fn to_b58(&self) -> String {
        bs58::encode(&self.0).into_string()
    }
}
