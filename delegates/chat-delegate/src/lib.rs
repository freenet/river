use freenet_stdlib::prelude::{delegate, ApplicationMessage, DelegateContext, DelegateError, DelegateInterface, GetSecretRequest, InboundDelegateMsg, OutboundDelegateMsg, Parameters, SecretsId, SetSecretRequest};
use river_common::chat_delegate::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Constants
const KEY_INDEX_SUFFIX: &str = "::key_index";
const ORIGIN_KEY_SEPARATOR: &str = ":";

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
        // Verify that attested is provided - this is the authenticated origin
        let origin: Origin = match attested {
            Some(origin) => {
                Origin(origin.to_vec())
            },
            // Can this error include the message type? AI!
            None => return Err(DelegateError::Other("missing attested origin".into())),
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
            InboundDelegateMsg::GetSecretResponse(response) => {
                handle_get_secret_response(response)
            }
            InboundDelegateMsg::UserResponse(_) => {
                // We don't use user responses in this delegate
                Ok(vec![])
            }
            InboundDelegateMsg::GetSecretRequest(_) => {
                // We don't handle direct get secret requests
                Err(DelegateError::Other("unexpected message type: get secret request".into()))
            }
        }
    }
}

/// Context for the chat delegate, storing pending operations.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ChatDelegateContext {
    /// Map of app-specific keys to (app_id, original_key, is_delete) for pending get requests
    /// The is_delete flag indicates whether this is a delete operation
    pending_gets: HashMap<String, (Origin, Vec<u8>, bool)>,
}

/// Structure to store the index of keys for an app
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct KeyIndex {
    keys: Vec<Vec<u8>>,
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

/// Handle an application message
fn handle_application_message(
    app_msg: ApplicationMessage,
    origin: &Origin,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    let mut context = ChatDelegateContext::try_from(app_msg.context)?;

    // Deserialize the request message
    let request: ChatDelegateRequestMsg = ciborium::from_reader(app_msg.payload.as_slice())
        .map_err(|e| DelegateError::Deser(format!("Failed to deserialize request: {e}")))?;

    // Create app-specific key prefix using the authenticated origin
    let _app_id = origin.to_b58(); // Prefix with underscore to indicate intentionally unused

    match request {
        ChatDelegateRequestMsg::StoreRequest { key, value } => {
            handle_store_request(&mut context, origin, key, value)
        }
        ChatDelegateRequestMsg::GetRequest { key } => {
            handle_get_request(&mut context, origin, key)
        }
        ChatDelegateRequestMsg::DeleteRequest { key } => {
            handle_delete_request(&mut context, origin, key)
        }
        ChatDelegateRequestMsg::ListRequest => {
            handle_list_request(&mut context, origin)
        }
    }
}

/// Handle a store request
fn handle_store_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    key: Vec<u8>,
    value: Vec<u8>,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this app's data
    let app_key = create_app_key(origin, &key);
    let secret_id = SecretsId::new(app_key.as_bytes().to_vec());

    // Create the index key
    let index_key = create_index_key(origin);

    // Store the original request in context for later processing after we get the index
    // This is a store operation, so is_delete = false
    context
        .pending_gets
        .insert(index_key.clone(), (origin.clone(), key.clone(), false));

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
    let get_index = create_get_index_request(&index_key, &context_bytes)?;

    // Return all messages
    Ok(vec![app_response, set_secret, get_index])
}

/// Handle a get request
fn handle_get_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    key: Vec<u8>,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this app's data
    let app_key = create_app_key(origin, &key);

    // Store the original request in context for later processing
    // This is a get operation, not a delete
    context
        .pending_gets
        .insert(app_key.clone(), (origin.clone(), key, false));

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create and return the get request
    let get_secret = create_get_request(&app_key, &context_bytes)?;

    Ok(vec![get_secret])
}

/// Handle a delete request
fn handle_delete_request(
    context: &mut ChatDelegateContext,
    origin: &Origin,
    key: Vec<u8>,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Create a unique key for this app's data
    let app_key = create_app_key(origin, &key);
    let secret_id = SecretsId::new(app_key.as_bytes().to_vec());

    // Create the index key
    let index_key = create_index_key(origin);

    // Store the original request in context for later processing after we get the index
    // This is a delete operation, so is_delete = true
    context
        .pending_gets
        .insert(index_key.clone(), (origin.clone(), key.clone(), true));

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
    let get_index = create_get_index_request(&index_key, &context_bytes)?;

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
    // Empty Vec<u8> indicates a list request
    context
        .pending_gets
        .insert(index_key.clone(), (origin.clone(), Vec::new(), false));

    // Serialize context
    let context_bytes = DelegateContext::try_from(&*context)?;

    // Create and return the get index request
    let get_index = create_get_index_request(&index_key, &context_bytes)?;

    Ok(vec![get_index])
}

/// Handle a get secret response
fn handle_get_secret_response(
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // Deserialize context
    let mut context = ChatDelegateContext::try_from(get_secret_response.context.clone())?;

    // Get the app_key from the secret ID
    let app_key = String::from_utf8(get_secret_response.key.key().to_vec())
        .map_err(|e| DelegateError::Other(format!("Invalid UTF-8 in key: {e}")))?;

    // Check if this is a key index response
    if app_key.ends_with(KEY_INDEX_SUFFIX) {
        handle_key_index_response(&app_key, &mut context, get_secret_response)
    } else {
        handle_regular_get_response(&app_key, &mut context, get_secret_response)
    }
}

/// Helper function to create a unique app key
fn create_app_key(origin: &Origin, key: &[u8]) -> String {
    format!("{}{}{}", origin.to_b58(), ORIGIN_KEY_SEPARATOR, bs58::encode(key).into_string())
}

/// Helper function to create an index key
fn create_index_key(origin: &Origin) -> String {
    format!("{}{}{}", origin.to_b58(), ORIGIN_KEY_SEPARATOR, KEY_INDEX_SUFFIX)
}

/// Helper function to create a get request
fn create_get_request(
    app_key: &str,
    context: &DelegateContext,
) -> Result<OutboundDelegateMsg, DelegateError> {
    let secret_id = SecretsId::new(app_key.as_bytes().to_vec());
    let get_secret = OutboundDelegateMsg::GetSecretRequest(GetSecretRequest {
        key: secret_id,
        context: context.clone(),
        processed: false,
    });

    Ok(get_secret)
}

/// Helper function to create a get index request
fn create_get_index_request(
    index_key: &str,
    context: &DelegateContext,
) -> Result<OutboundDelegateMsg, DelegateError> {
    let index_secret_id = SecretsId::new(index_key.as_bytes().to_vec());
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
    app_key: &str,
    context: &mut ChatDelegateContext,
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // This is a response to a key index request
    if let Some((_app_id, original_key, is_delete)) = context.pending_gets.get(app_key).cloned() {
        let mut outbound_msgs = Vec::new();

        // Parse the key index or create a new one if it doesn't exist
        let mut key_index = if let Some(index_data) = &get_secret_response.value {
            ciborium::from_reader(index_data.as_slice()).map_err(|e| {
                DelegateError::Deser(format!("Failed to deserialize key index: {e}"))
            })?
        } else {
            KeyIndex::default()
        };

        // If original_key is empty, this is a ListRequest
        if original_key.is_empty() {
            // Create list response
            let response = ChatDelegateResponseMsg::ListResponse {
                keys: key_index.keys.clone(),
            };

            // Create response message
            let context_bytes = DelegateContext::try_from(&ChatDelegateContext::default())?;
            let app_response = create_app_response(&response, &context_bytes)?;
            outbound_msgs.push(app_response);
        } else {
            // This is a store or delete operation that needs to update the index

            if is_delete {
                // For delete operations, remove the key
                key_index.keys.retain(|k| k != &original_key);
            } else {
                // For store operations, add the key if it doesn't exist
                if !key_index.keys.contains(&original_key) {
                    key_index.keys.push(original_key.clone());
                }
            }

            // Serialize the updated index
            let mut index_bytes = Vec::new();
            ciborium::ser::into_writer(&key_index, &mut index_bytes)
                .map_err(|e| DelegateError::Deser(format!("Failed to serialize key index: {e}")))?;

            // Create set secret request to update the index
            let set_index = OutboundDelegateMsg::SetSecretRequest(SetSecretRequest {
                key: SecretsId::new(app_key.as_bytes().to_vec()),
                value: Some(index_bytes),
            });

            outbound_msgs.push(set_index);
        }

        // Remove the pending get request
        context.pending_gets.remove(app_key);

        Ok(outbound_msgs)
    } else {
        // No pending get request for this key index
        Err(DelegateError::Other(format!(
            "No pending key index request for: {app_key}"
        )))
    }
}

/// Handle a regular get response
fn handle_regular_get_response(
    app_key: &str,
    context: &mut ChatDelegateContext,
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    if let Some((_app_id, original_key, _)) = context.pending_gets.get(app_key).cloned() {
        // Create response
        let response = ChatDelegateResponseMsg::GetResponse {
            key: original_key,
            value: get_secret_response.value,
        };

        // Create response message
        let context_bytes = DelegateContext::try_from(&ChatDelegateContext::default())?;
        let app_response = create_app_response(&response, &context_bytes)?;

        // Remove the pending get request
        context.pending_gets.remove(app_key);

        Ok(vec![app_response])
    } else {
        Err(DelegateError::Other(format!(
            "No pending get request for key: {app_key}"
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
    
    // ContractInstanceId is no longer used, so we don't need this extension trait

    /// Helper function to create an application message
    fn create_app_message(
        request: ChatDelegateRequestMsg,
    ) -> ApplicationMessage {
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
            key: key.clone(),
            value: value.clone(),
        };

        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = ChatDelegate::process(create_test_parameters(), Some(get_test_origin_bytes()), inbound_msg).unwrap();

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
                assert_eq!(resp_key, key);
                assert!(result.is_ok());
                assert_eq!(value_size, value.len());
            }
            _ => panic!("Expected StoreResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_request() {
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::GetRequest { key: key.clone() };
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = ChatDelegate::process(create_test_parameters(), Some(get_test_origin_bytes()), inbound_msg).unwrap();

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

        let request = ChatDelegateRequestMsg::DeleteRequest { key: key.clone() };
        let app_msg = create_app_message(request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = ChatDelegate::process(create_test_parameters(), Some(get_test_origin_bytes()), inbound_msg).unwrap();

        // Should have 3 messages: app response, set secret (with None value), get index
        assert_eq!(result.len(), 3);

        // Check app response
        let response = extract_response(result.clone()).unwrap();
        match response {
            ChatDelegateResponseMsg::DeleteResponse {
                key: resp_key,
                result,
            } => {
                assert_eq!(resp_key, key);
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

        let result = ChatDelegate::process(create_test_parameters(), Some(get_test_origin_bytes()), inbound_msg).unwrap();

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

        let app_key = create_app_key(&test_origin, &key);
        context
            .pending_gets
            .insert(app_key.clone(), (test_origin.clone(), key.clone(), false));

        // Serialize the context
        let context_bytes = DelegateContext::try_from(&context)
            .map_err(|e| panic!("Failed to serialize context: {e}"))
            .unwrap();

        // Create a get secret response
        let secret_id = SecretsId::new(app_key.as_bytes().to_vec());
        let get_response = freenet_stdlib::prelude::GetSecretResponse {
            key: secret_id,
            value: Some(value.clone()),
            context: context_bytes,
        };

        let inbound_msg = InboundDelegateMsg::GetSecretResponse(get_response);

        // Pass the attested origin parameter
        let result = ChatDelegate::process(create_test_parameters(), Some(get_test_origin_bytes()), inbound_msg).unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check app response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::GetResponse {
                key: resp_key,
                value: resp_value,
            } => {
                assert_eq!(resp_key, key);
                assert_eq!(resp_value, Some(value));
            }
            _ => panic!("Expected GetResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_secret_response_for_list_request() {
        let keys = vec![b"key1".to_vec(), b"key2".to_vec(), b"key3".to_vec()];

        // Create a key index with some keys
        let key_index = KeyIndex { keys: keys.clone() };
        let mut index_bytes = Vec::new();
        ciborium::ser::into_writer(&key_index, &mut index_bytes)
            .map_err(|e| panic!("Failed to serialize key index: {e}"))
            .unwrap();

        let test_origin = create_test_origin();

        // Create a context with a pending list request
        let mut context = ChatDelegateContext::default();
        let index_key = create_index_key(&test_origin);
        context
            .pending_gets
            .insert(index_key.clone(), (test_origin.clone(), Vec::new(), false));

        // Serialize the context
        let context_bytes = DelegateContext::try_from(&context)
            .map_err(|e| panic!("Failed to serialize context: {e}"))
            .unwrap();

        // Create a get secret response for the index
        let secret_id = SecretsId::new(index_key.as_bytes().to_vec());
        let get_response = freenet_stdlib::prelude::GetSecretResponse {
            key: secret_id,
            value: Some(index_bytes),
            context: context_bytes,
        };

        let inbound_msg = InboundDelegateMsg::GetSecretResponse(get_response);

        // Pass the attested origin parameter
        let result = ChatDelegate::process(create_test_parameters(), Some(get_test_origin_bytes()), inbound_msg).unwrap();

        // Should have 1 message: app response
        assert_eq!(result.len(), 1);

        // Check app response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::ListResponse { keys: resp_keys } => {
                assert_eq!(resp_keys, keys);
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
            keys: existing_keys,
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
            .pending_gets
            .insert(index_key.clone(), (test_origin.clone(), key.clone(), false));

        // Serialize the context
        let context_bytes = DelegateContext::try_from(&context)
            .map_err(|e| panic!("Failed to serialize context: {e}"))
            .unwrap();

        // Create a get secret response for the index
        let secret_id = SecretsId::new(index_key.as_bytes().to_vec());
        let get_response = freenet_stdlib::prelude::GetSecretResponse {
            key: secret_id,
            value: Some(index_bytes),
            context: context_bytes,
        };

        let inbound_msg = InboundDelegateMsg::GetSecretResponse(get_response);

        // Pass the attested origin parameter
        let result = ChatDelegate::process(create_test_parameters(), Some(get_test_origin_bytes()), inbound_msg).unwrap();

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
                assert!(updated_index.keys.contains(&key));
                assert!(updated_index.keys.contains(&b"existing_key".to_vec()));
            }
            _ => panic!("Expected SetSecretRequest, got {:?}", result[0]),
        }
    }
    
    #[test]
    fn test_error_on_processed_message() {
        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        let request = ChatDelegateRequestMsg::StoreRequest {
            key,
            value,
        };

        let mut app_msg = create_app_message(request);
        app_msg = app_msg.processed(true); // Mark as already processed
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let _origin = create_test_origin();

        let result = ChatDelegate::process(create_test_parameters(), Some(get_test_origin_bytes()), inbound_msg);
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
        
        let result = ChatDelegate::process(create_test_parameters(), Some(get_test_origin_bytes()), inbound_msg);
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

        let request = ChatDelegateRequestMsg::GetRequest { key };
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Origin(Vec<u8>);

impl Origin {
    fn to_b58(&self) -> String {
        bs58::encode(&self.0).into_string()
    }
}
