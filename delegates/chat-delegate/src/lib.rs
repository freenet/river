use freenet_stdlib::prelude::{
    delegate, ApplicationMessage, DelegateContext, DelegateError, DelegateInterface,
    GetSecretRequest, InboundDelegateMsg, OutboundDelegateMsg, Parameters, SecretsId,
    SetSecretRequest,
};
use river_common::chat_delegate::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub struct ChatDelegate;

// Constants for key index
const KEY_INDEX_SUFFIX: &str = "::key_index";

#[delegate]
impl DelegateInterface for ChatDelegate {
    fn process(
        _parameters: Parameters<'static>,
        _attested: Option<&'static [u8]>,
        message: InboundDelegateMsg,
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        match message {
            InboundDelegateMsg::ApplicationMessage(app_msg) => {
                let mut context = deserialize_context(app_msg.context.as_ref())?;

                // Deserialize the request message
                let request: ChatDelegateRequestMsg =
                    ciborium::from_reader(app_msg.payload.as_slice()).map_err(|e| {
                        DelegateError::Deser(format!("Failed to deserialize request: {e}"))
                    })?;

                // Create app-specific key prefix
                let app_id = app_msg.app.to_string();

                match request {
                    ChatDelegateRequestMsg::StoreRequest { key, value } => {
                        // Create a unique key for this app's data
                        let app_key = create_app_key(&app_id, &key);
                        let secret_id = SecretsId::new(app_key.as_bytes().to_vec());

                        // Create the index key
                        let index_key = create_index_key(&app_id);

                        // Store the original request in context for later processing after we get the index
                        // This is a store operation, so is_delete = false
                        context
                            .pending_gets
                            .insert(index_key.clone(), (app_msg.app, key.clone(), false));

                        // Create response for the client
                        let response = ChatDelegateResponseMsg::StoreResponse {
                            key: key.clone(),
                            result: Ok(()),
                        };

                        // Serialize context
                        let context_bytes = serialize_context(&context)?;

                        // Create the three messages we need to send:
                        // 1. Response to the client
                        let app_response =
                            create_app_response(app_msg.app, &response, &context_bytes)?;

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

                    ChatDelegateRequestMsg::GetRequest { key } => {
                        // Create a unique key for this app's data
                        let app_key = create_app_key(&app_id, &key);

                        // Store the original request in context for later processing
                        // This is a get operation, not a delete
                        context
                            .pending_gets
                            .insert(app_key.clone(), (app_msg.app, key, false));

                        // Serialize context
                        let context_bytes = serialize_context(&context)?;

                        // Create and return the get request
                        let get_secret = create_get_request(&app_key, &context_bytes)?;

                        Ok(vec![get_secret])
                    }

                    ChatDelegateRequestMsg::DeleteRequest { key } => {
                        // Create a unique key for this app's data
                        let app_key = create_app_key(&app_id, &key);
                        let secret_id = SecretsId::new(app_key.clone().as_bytes().to_vec());

                        // Create the index key
                        let index_key = create_index_key(&app_id);

                        // Store the original request in context for later processing after we get the index
                        // This is a delete operation, so is_delete = true
                        context
                            .pending_gets
                            .insert(index_key.clone(), (app_msg.app, key.clone(), true));

                        // Create response for the client
                        let response = ChatDelegateResponseMsg::DeleteResponse {
                            key: key.clone(),
                            result: Ok(()),
                        };

                        // Serialize context
                        let context_bytes = serialize_context(&context)?;

                        // Create the three messages we need to send:
                        // 1. Response to the client
                        let app_response =
                            create_app_response(app_msg.app, &response, &context_bytes)?;

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

                    ChatDelegateRequestMsg::ListRequest => {
                        // Create the index key
                        let index_key = create_index_key(&app_id);

                        // Store a special marker in the context to indicate this is a list request
                        // Empty Vec<u8> indicates a list request
                        context
                            .pending_gets
                            .insert(index_key.clone(), (app_msg.app, Vec::new(), false));

                        // Serialize context
                        let context_bytes = serialize_context(&context)?;

                        // Create and return the get index request
                        let get_index = create_get_index_request(&index_key, &context_bytes)?;

                        Ok(vec![get_index])
                    }
                }
            }

            InboundDelegateMsg::GetSecretResponse(get_secret_response) => {
                // Deserialize context
                let mut context = deserialize_context(get_secret_response.context.as_ref())?;

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

            InboundDelegateMsg::UserResponse(_user_response) => {
                // We don't use user responses in this delegate
                Ok(vec![])
            }

            InboundDelegateMsg::GetSecretRequest(_get_secret_request) => {
                // We don't handle direct get secret requests
                Ok(vec![])
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ChatDelegateContext {
    // Map of app-specific keys to (app_id, original_key, is_delete) for pending get requests
    // The is_delete flag indicates whether this is a delete operation
    pending_gets: HashMap<String, (freenet_stdlib::prelude::ContractInstanceId, Vec<u8>, bool)>,
    // We don't need to store rooms in the context anymore as we use the secret storage
}

// Structure to store the index of keys for an app
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct KeyIndex {
    keys: Vec<Vec<u8>>,
}

// Helper function to deserialize context or create a default one
fn deserialize_context(context_bytes: &[u8]) -> Result<ChatDelegateContext, DelegateError> {
    if context_bytes.is_empty() {
        Ok(ChatDelegateContext::default())
    } else {
        ciborium::from_reader(context_bytes)
            .map_err(|e| DelegateError::Deser(format!("Failed to deserialize context: {e}")))
    }
}

// Helper function to serialize context
fn serialize_context(context: &ChatDelegateContext) -> Result<Vec<u8>, DelegateError> {
    let mut context_bytes = Vec::new();
    ciborium::ser::into_writer(context, &mut context_bytes)
        .map_err(|e| DelegateError::Deser(format!("Failed to serialize context: {e}")))?;
    Ok(context_bytes)
}

// Helper function to create a unique app key
fn create_app_key(app_id: &str, key: &[u8]) -> String {
    format!("{}:{}", app_id, bs58::encode(key).into_string())
}

// Helper function to create an index key
fn create_index_key(app_id: &str) -> String {
    format!("{}:{}", app_id, KEY_INDEX_SUFFIX)
}

// Helper function to create a get request
fn create_get_request(
    app_key: &str,
    context_bytes: &[u8],
) -> Result<OutboundDelegateMsg, DelegateError> {
    let secret_id = SecretsId::new(app_key.as_bytes().to_vec());
    let mut get_secret = OutboundDelegateMsg::GetSecretRequest(GetSecretRequest {
        key: secret_id,
        context: DelegateContext::default(),
        processed: false,
    });

    if let Some(ctx) = get_secret.get_mut_context() {
        ctx.replace(context_bytes.to_vec());
    }

    Ok(get_secret)
}

// Helper function to create a get index request
fn create_get_index_request(
    index_key: &str,
    context_bytes: &[u8],
) -> Result<OutboundDelegateMsg, DelegateError> {
    let index_secret_id = SecretsId::new(index_key.as_bytes().to_vec());
    let mut get_index = OutboundDelegateMsg::GetSecretRequest(GetSecretRequest {
        key: index_secret_id,
        context: DelegateContext::default(),
        processed: false,
    });

    if let Some(ctx) = get_index.get_mut_context() {
        ctx.replace(context_bytes.to_vec());
    }

    Ok(get_index)
}

// Helper function to create an app response
fn create_app_response<T: Serialize>(
    app_id: freenet_stdlib::prelude::ContractInstanceId,
    response: &T,
    context_bytes: &[u8],
) -> Result<OutboundDelegateMsg, DelegateError> {
    // Serialize response
    let mut response_bytes = Vec::new();
    ciborium::ser::into_writer(response, &mut response_bytes)
        .map_err(|e| DelegateError::Deser(format!("Failed to serialize response: {e}")))?;

    // Create response message
    Ok(OutboundDelegateMsg::ApplicationMessage(
        ApplicationMessage::new(app_id, response_bytes)
            .with_context(DelegateContext::new(context_bytes.to_vec()))
            .processed(true),
    ))
}

// Handle a key index response
fn handle_key_index_response(
    app_key: &str,
    context: &mut ChatDelegateContext,
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // This is a response to a key index request
    if let Some((app_id, original_key, is_delete)) = context.pending_gets.get(app_key).cloned() {
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
            let app_response = create_app_response(app_id, &response, &[])?;
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

// Handle a regular get response
fn handle_regular_get_response(
    app_key: &str,
    context: &mut ChatDelegateContext,
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse,
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    if let Some((app_id, original_key, _)) = context.pending_gets.get(app_key) {
        // Create response
        let response = ChatDelegateResponseMsg::GetResponse {
            key: original_key.clone(),
            value: get_secret_response.value,
        };

        // Create response message
        let app_response = create_app_response(*app_id, &response, &[])?;

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
    use freenet_stdlib::prelude::{ContractInstanceId, DelegateContext};

    // Helper function to create empty parameters for testing
    fn create_test_parameters() -> Parameters<'static> {
        Parameters::from(vec![])
    }

    // Helper function to create a test app ID
    fn create_test_app_id() -> ContractInstanceId {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        ContractInstanceId::new(bytes)
    }

    // Helper function to create an application message
    fn create_app_message(
        app_id: ContractInstanceId,
        request: ChatDelegateRequestMsg,
    ) -> ApplicationMessage {
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&request, &mut payload).unwrap();
        ApplicationMessage::new(app_id, payload)
    }

    // Helper function to extract response from outbound messages
    fn extract_response(messages: Vec<OutboundDelegateMsg>) -> Option<ChatDelegateResponseMsg> {
        for msg in messages {
            if let OutboundDelegateMsg::ApplicationMessage(app_msg) = msg {
                return Some(ciborium::from_reader(app_msg.payload.as_slice()).unwrap());
            }
        }
        None
    }

    #[test]
    fn test_store_request() {
        let app_id = create_test_app_id();
        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        let request = ChatDelegateRequestMsg::StoreRequest {
            key: key.clone(),
            value: value.clone(),
        };

        let app_msg = create_app_message(app_id, request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = ChatDelegate::process(create_test_parameters(), None, inbound_msg).unwrap();

        // Should have 3 messages: app response, set secret, get index
        assert_eq!(result.len(), 3);

        // Check app response
        let response = extract_response(result).unwrap();
        match response {
            ChatDelegateResponseMsg::StoreResponse {
                key: resp_key,
                result,
            } => {
                assert_eq!(resp_key, key);
                assert!(result.is_ok());
            }
            _ => panic!("Expected StoreResponse, got {:?}", response),
        }
    }

    #[test]
    fn test_get_request() {
        let app_id = create_test_app_id();
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::GetRequest { key: key.clone() };
        let app_msg = create_app_message(app_id, request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = ChatDelegate::process(create_test_parameters(), None, inbound_msg).unwrap();

        // Should have 1 message: get secret request
        assert_eq!(result.len(), 1);

        // Check it's a get secret request
        match &result[0] {
            OutboundDelegateMsg::GetSecretRequest(req) => {
                // Verify the key contains our app ID and key
                let key_str = String::from_utf8(req.key.key().to_vec()).unwrap();
                assert!(key_str.contains(&app_id.to_string()));
                assert!(key_str.contains(&bs58::encode(&key).into_string()));
            }
            _ => panic!("Expected GetSecretRequest, got {:?}", result[0]),
        }
    }

    #[test]
    fn test_delete_request() {
        let app_id = create_test_app_id();
        let key = b"test_key".to_vec();

        let request = ChatDelegateRequestMsg::DeleteRequest { key: key.clone() };
        let app_msg = create_app_message(app_id, request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = ChatDelegate::process(create_test_parameters(), None, inbound_msg).unwrap();

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
        let app_id = create_test_app_id();

        let request = ChatDelegateRequestMsg::ListRequest;
        let app_msg = create_app_message(app_id, request);
        let inbound_msg = InboundDelegateMsg::ApplicationMessage(app_msg);

        let result = ChatDelegate::process(create_test_parameters(), None, inbound_msg).unwrap();

        // Should have 1 message: get index request
        assert_eq!(result.len(), 1);

        // Check it's a get secret request for the index
        match &result[0] {
            OutboundDelegateMsg::GetSecretRequest(req) => {
                // Verify the key contains our app ID and key_index suffix
                let key_str = String::from_utf8(req.key.key().to_vec()).unwrap();
                assert!(key_str.contains(&app_id.to_string()));
                assert!(key_str.contains(KEY_INDEX_SUFFIX));
            }
            _ => panic!("Expected GetSecretRequest, got {:?}", result[0]),
        }
    }

    #[test]
    fn test_get_secret_response_for_regular_get() {
        let app_id = create_test_app_id();
        let key = b"test_key".to_vec();
        let value = b"test_value".to_vec();

        // Create a context with a pending get
        let mut context = ChatDelegateContext::default();
        let app_key = create_app_key(&app_id.to_string(), &key);
        context
            .pending_gets
            .insert(app_key.clone(), (app_id, key.clone(), false));

        // Serialize the context
        let context_bytes = serialize_context(&context).unwrap();

        // Create a get secret response
        let secret_id = SecretsId::new(app_key.as_bytes().to_vec());
        let get_response = freenet_stdlib::prelude::GetSecretResponse {
            key: secret_id,
            value: Some(value.clone()),
            context: DelegateContext::new(context_bytes),
        };

        let inbound_msg = InboundDelegateMsg::GetSecretResponse(get_response);

        let result = ChatDelegate::process(create_test_parameters(), None, inbound_msg).unwrap();

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
        let app_id = create_test_app_id();
        let keys = vec![b"key1".to_vec(), b"key2".to_vec(), b"key3".to_vec()];

        // Create a key index with some keys
        let key_index = KeyIndex { keys: keys.clone() };
        let mut index_bytes = Vec::new();
        ciborium::ser::into_writer(&key_index, &mut index_bytes).unwrap();

        // Create a context with a pending list request
        let mut context = ChatDelegateContext::default();
        let index_key = create_index_key(&app_id.to_string());
        context
            .pending_gets
            .insert(index_key.clone(), (app_id, Vec::new(), false));

        // Serialize the context
        let context_bytes = serialize_context(&context).unwrap();

        // Create a get secret response for the index
        let secret_id = SecretsId::new(index_key.as_bytes().to_vec());
        let get_response = freenet_stdlib::prelude::GetSecretResponse {
            key: secret_id,
            value: Some(index_bytes),
            context: DelegateContext::new(context_bytes),
        };

        let inbound_msg = InboundDelegateMsg::GetSecretResponse(get_response);

        let result = ChatDelegate::process(create_test_parameters(), None, inbound_msg).unwrap();

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
        let app_id = create_test_app_id();
        let key = b"test_key".to_vec();

        // Create a key index with some existing keys
        let existing_keys = vec![b"existing_key".to_vec()];
        let key_index = KeyIndex {
            keys: existing_keys,
        };
        let mut index_bytes = Vec::new();
        ciborium::ser::into_writer(&key_index, &mut index_bytes).unwrap();

        // Create a context with a pending store request
        let mut context = ChatDelegateContext::default();
        let index_key = create_index_key(&app_id.to_string());
        context
            .pending_gets
            .insert(index_key.clone(), (app_id, key.clone(), false));

        // Serialize the context
        let context_bytes = serialize_context(&context).unwrap();

        // Create a get secret response for the index
        let secret_id = SecretsId::new(index_key.as_bytes().to_vec());
        let get_response = freenet_stdlib::prelude::GetSecretResponse {
            key: secret_id,
            value: Some(index_bytes),
            context: DelegateContext::new(context_bytes),
        };

        let inbound_msg = InboundDelegateMsg::GetSecretResponse(get_response);

        let result = ChatDelegate::process(create_test_parameters(), None, inbound_msg).unwrap();

        // Should have 1 message: set secret request to update the index
        assert_eq!(result.len(), 1);

        // Check it's a set secret request
        match &result[0] {
            OutboundDelegateMsg::SetSecretRequest(req) => {
                // Deserialize the value to check the updated index
                let updated_index: KeyIndex =
                    ciborium::from_reader(req.value.as_ref().unwrap().as_slice()).unwrap();

                // Should contain both the existing key and our new key
                assert_eq!(updated_index.keys.len(), 2);
                assert!(updated_index.keys.contains(&key));
                assert!(updated_index.keys.contains(&b"existing_key".to_vec()));
            }
            _ => panic!("Expected SetSecretRequest, got {:?}", result[0]),
        }
    }
}
