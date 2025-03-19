use std::collections::HashMap;
use freenet_stdlib::prelude::{
    delegate, ApplicationMessage, DelegateContext, DelegateError, DelegateInterface, GetSecretRequest,
    InboundDelegateMsg, OutboundDelegateMsg, Parameters, SecretsId, SetSecretRequest,
};
use serde::{Deserialize, Serialize};
use river_common::chat_delegate::*;

pub struct ChatDelegate;

// Constants for key index
const KEY_INDEX_SUFFIX: &str = "::key_index";

#[delegate]
impl DelegateInterface for ChatDelegate {
    fn process(
        _parameters: Parameters<'static>,
        _attested: Option<&'static [u8]>,
        message: InboundDelegateMsg
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        match message {
            InboundDelegateMsg::ApplicationMessage(app_msg) => {
                let mut context = deserialize_context(app_msg.context.as_ref())?;

                // Deserialize the request message
                let request: ChatDelegateRequestMsg = ciborium::from_reader(app_msg.payload.as_slice())
                    .map_err(|e| DelegateError::Deser(format!("Failed to deserialize request: {e}")))?;

                // Create app-specific key prefix
                let app_id = app_msg.app.to_string();
                
                match request {
                    ChatDelegateRequestMsg::StoreRequest { key, value } => {
                        // Create a unique key for this app's data
                        let app_key = create_app_key(&app_id, &key);
                        let secret_id = SecretsId::new(app_key.into_bytes());
                        
                        // Create the index key
                        let index_key = create_index_key(&app_id);
                        
                        // Store the original request in context for later processing after we get the index
                        context.pending_gets.insert(index_key.clone(), (app_msg.app, key.clone()));
                        
                        // Create response for the client
                        let response = ChatDelegateResponseMsg::StoreResponse {
                            key: key.clone(),
                            result: Ok(()),
                        };
                        
                        // Serialize context
                        let context_bytes = serialize_context(&context)?;
                        
                        // Create the three messages we need to send:
                        // 1. Response to the client
                        let app_response = create_app_response(app_msg.app, &response, &context_bytes)?;
                        
                        // 2. Store the actual value
                        let set_secret = OutboundDelegateMsg::SetSecretRequest(
                            SetSecretRequest {
                                key: secret_id,
                                value: Some(value),
                            }
                        );
                        
                        // 3. Request the current index to update it
                        let get_index = create_get_index_request(&index_key, &context_bytes)?;
                        
                        // Return all messages
                        Ok(vec![app_response, set_secret, get_index])
                    },
                    
                    ChatDelegateRequestMsg::GetRequest { key } => {
                        // Create a unique key for this app's data
                        let app_key = create_app_key(&app_id, &key);
                        
                        // Store the original request in context for later processing
                        context.pending_gets.insert(app_key.clone(), (app_msg.app, key));
                        
                        // Serialize context
                        let context_bytes = serialize_context(&context)?;
                        
                        // Create and return the get request
                        let get_secret = create_get_request(&app_key, &context_bytes)?;
                        
                        Ok(vec![get_secret])
                    },
                    
                    ChatDelegateRequestMsg::DeleteRequest { key } => {
                        // Create a unique key for this app's data
                        let app_key = create_app_key(&app_id, &key);
                        let secret_id = SecretsId::new(app_key.clone().into_bytes());
                        
                        // Create the index key
                        let index_key = create_index_key(&app_id);
                        
                        // Store the original request in context for later processing after we get the index
                        context.pending_gets.insert(index_key.clone(), (app_msg.app, key.clone()));
                        
                        // Create response for the client
                        let response = ChatDelegateResponseMsg::DeleteResponse {
                            key: key.clone(),
                            result: Ok(()),
                        };
                        
                        // Serialize context
                        let context_bytes = serialize_context(&context)?;
                        
                        // Create the three messages we need to send:
                        // 1. Response to the client
                        let app_response = create_app_response(app_msg.app, &response, &context_bytes)?;
                        
                        // 2. Delete the actual value
                        let set_secret = OutboundDelegateMsg::SetSecretRequest(
                            SetSecretRequest {
                                key: secret_id,
                                value: None, // Setting to None deletes the secret
                            }
                        );
                        
                        // 3. Request the current index to update it
                        let get_index = create_get_index_request(&index_key, &context_bytes)?;
                        
                        // Return all messages
                        Ok(vec![app_response, set_secret, get_index])
                    },

                    ChatDelegateRequestMsg::ListRequest => {
                        // Create the index key
                        let index_key = create_index_key(&app_id);
                        
                        // Store a special marker in the context to indicate this is a list request
                        // Empty Vec<u8> indicates a list request
                        context.pending_gets.insert(index_key.clone(), (app_msg.app, Vec::new()));
                        
                        // Serialize context
                        let context_bytes = serialize_context(&context)?;
                        
                        // Create and return the get index request
                        let get_index = create_get_index_request(&index_key, &context_bytes)?;
                        
                        Ok(vec![get_index])
                    },
                }
            },
            
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
            },
            
            InboundDelegateMsg::UserResponse(_user_response) => {
                // We don't use user responses in this delegate
                Ok(vec![])
            },
            
            InboundDelegateMsg::GetSecretRequest(_get_secret_request) => {
                // We don't handle direct get secret requests
                Ok(vec![])
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ChatDelegateContext {
    // Map of app-specific keys to (app_id, original_key) for pending get requests
    pending_gets: HashMap<String, (freenet_stdlib::prelude::ContractInstanceId, Vec<u8>)>,
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
fn create_get_request(app_key: &str, context_bytes: &[u8]) -> Result<OutboundDelegateMsg, DelegateError> {
    let secret_id = SecretsId::new(app_key.clone().into_bytes());
    let mut get_secret = OutboundDelegateMsg::GetSecretRequest(
        GetSecretRequest {
            key: secret_id,
            context: DelegateContext::default(),
            processed: false,
        }
    );
    
    if let Some(ctx) = get_secret.get_mut_context() {
        ctx.replace(context_bytes.to_vec());
    }
    
    Ok(get_secret)
}

// Helper function to create a get index request
fn create_get_index_request(index_key: &str, context_bytes: &[u8]) -> Result<OutboundDelegateMsg, DelegateError> {
    let index_secret_id = SecretsId::new(index_key.clone().into_bytes());
    let mut get_index = OutboundDelegateMsg::GetSecretRequest(
        GetSecretRequest {
            key: index_secret_id,
            context: DelegateContext::default(),
            processed: false,
        }
    );
    
    if let Some(ctx) = get_index.get_mut_context() {
        ctx.replace(context_bytes.to_vec());
    }
    
    Ok(get_index)
}

// Helper function to create an app response
fn create_app_response<T: Serialize>(
    app_id: freenet_stdlib::prelude::ContractInstanceId,
    response: &T,
    context_bytes: &[u8]
) -> Result<OutboundDelegateMsg, DelegateError> {
    // Serialize response
    let mut response_bytes = Vec::new();
    ciborium::ser::into_writer(response, &mut response_bytes)
        .map_err(|e| DelegateError::Deser(format!("Failed to serialize response: {e}")))?;
    
    // Create response message
    Ok(OutboundDelegateMsg::ApplicationMessage(
        ApplicationMessage::new(app_id, response_bytes)
            .with_context(DelegateContext::new(context_bytes.to_vec()))
            .processed(true)
    ))
}

// Handle a key index response
fn handle_key_index_response(
    app_key: &str,
    context: &mut ChatDelegateContext,
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    // This is a response to a key index request
    if let Some((app_id, original_key)) = context.pending_gets.get(app_key).cloned() {
        let mut outbound_msgs = Vec::new();
        
        // Parse the key index or create a new one if it doesn't exist
        let mut key_index = if let Some(index_data) = &get_secret_response.value {
            ciborium::from_reader(index_data.as_slice())
                .map_err(|e| DelegateError::Deser(format!("Failed to deserialize key index: {e}")))?
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
            
            // For store operations, add the key if it doesn't exist
            if !key_index.keys.contains(&original_key) {
                key_index.keys.push(original_key.clone());
            }
            
            // For delete operations, remove the key
            key_index.keys.retain(|k| k != &original_key);
            
            // Serialize the updated index
            let mut index_bytes = Vec::new();
            ciborium::ser::into_writer(&key_index, &mut index_bytes)
                .map_err(|e| DelegateError::Deser(format!("Failed to serialize key index: {e}")))?;
            
            // Create set secret request to update the index
            let set_index = OutboundDelegateMsg::SetSecretRequest(
                SetSecretRequest {
                    key: SecretsId::new(app_key.clone().into_bytes()),
                    value: Some(index_bytes),
                }
            );
            
            outbound_msgs.push(set_index);
        }
        
        // Remove the pending get request
        context.pending_gets.remove(app_key);
        
        Ok(outbound_msgs)
    } else {
        // No pending get request for this key index
        Err(DelegateError::Other(format!("No pending key index request for: {app_key}")))
    }
}

// Handle a regular get response
fn handle_regular_get_response(
    app_key: &str,
    context: &mut ChatDelegateContext,
    get_secret_response: freenet_stdlib::prelude::GetSecretResponse
) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
    if let Some((app_id, original_key)) = context.pending_gets.get(app_key) {
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
        Err(DelegateError::Other(format!("No pending get request for key: {app_key}")))
    }
}
