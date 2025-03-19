use std::collections::HashMap;
use freenet_stdlib::prelude::{
    delegate, ApplicationMessage, DelegateContext, DelegateError, DelegateInterface, GetSecretRequest,
    InboundDelegateMsg, OutboundDelegateMsg, Parameters, SecretsId, SetSecretRequest,
};
use serde::{Deserialize, Serialize};
use river_common::chat_delegate::*;

pub struct ChatDelegate;

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
                        let app_key = format!("{}:{}", app_id, bs58::encode(&key).into_string());
                        let secret_id = SecretsId::new(app_key.clone().into_bytes());
                        
                        // Update the key index
                        let mut outbound_msgs = Vec::new();
                        
                        // First, get the current key index
                        let index_key = format!("{}:{}", app_id, KEY_INDEX_SUFFIX);
                        let index_secret_id = SecretsId::new(index_key.clone().into_bytes());
                        
                        // Request the current index
                        let get_index = OutboundDelegateMsg::GetSecretRequest(
                            GetSecretRequest {
                                key: index_secret_id.clone(),
                                context: DelegateContext::default(),
                                processed: false,
                            }
                        );
                        
                        // Store the original request in context for later processing after we get the index
                        context.pending_gets.insert(index_key.clone(), (app_msg.app, key.clone()));
                        
                        // Create response
                        let response = ChatDelegateResponseMsg::StoreResponse {
                            key,
                            result: Ok(()),
                        };
                        
                        // Serialize response
                        let mut response_bytes = Vec::new();
                        ciborium::ser::into_writer(&response, &mut response_bytes)
                            .map_err(|e| DelegateError::Deser(format!("Failed to serialize response: {e}")))?;
                        
                        // Serialize context
                        let mut context_bytes = Vec::new();
                        ciborium::ser::into_writer(&context, &mut context_bytes)
                            .map_err(|e| DelegateError::Deser(format!("Failed to serialize context: {e}")))?;
                        
                        // Create response message and set secret request
                        let app_response = OutboundDelegateMsg::ApplicationMessage(
                            ApplicationMessage::new(app_msg.app, response_bytes)
                                .with_context(DelegateContext::new(context_bytes))
                                .processed(true)
                        );
                        
                        let set_secret = OutboundDelegateMsg::SetSecretRequest(
                            SetSecretRequest {
                                key: secret_id,
                                value: Some(value.clone()),
                            }
                        );
                        
                        // Update context in the get_index request
                        let mut get_index_msgs = vec![get_index];
                        if let Some(ctx) = get_index_msgs[0].get_mut_context() {
                            ctx.replace(context_bytes.clone());
                        }
                        
                        // We'll first get the index, then update it in the GetSecretResponse handler
                        outbound_msgs.push(set_secret);
                        outbound_msgs.push(get_index_msgs.remove(0));
                        
                        Ok(outbound_msgs)
                    },
                    
                    ChatDelegateRequestMsg::GetRequest { key } => {
                        // Create a unique key for this app's data
                        let app_key = format!("{}:{}", app_id, bs58::encode(&key).into_string());
                        let secret_id = SecretsId::new(app_key.clone().into_bytes());
                        
                        // Create get secret request
                        let get_secret = OutboundDelegateMsg::GetSecretRequest(
                            GetSecretRequest {
                                key: secret_id,
                                context: DelegateContext::default(),
                                processed: false,
                            }
                        );
                        
                        // Store the original request in context for later processing
                        context.pending_gets.insert(app_key, (app_msg.app, key));
                        
                        // Serialize context
                        let mut context_bytes = Vec::new();
                        ciborium::ser::into_writer(&context, &mut context_bytes)
                            .map_err(|e| DelegateError::Deser(format!("Failed to serialize context: {e}")))?;
                        
                        // Update context in the get_secret request
                        let mut get_secret_msgs = vec![get_secret];
                        if let Some(ctx) = get_secret_msgs[0].get_mut_context() {
                            ctx.replace(context_bytes);
                        }
                        
                        Ok(get_secret_msgs)
                    },
                    
                    ChatDelegateRequestMsg::DeleteRequest { key } => {
                        // Create a unique key for this app's data
                        let app_key = format!("{}:{}", app_id, bs58::encode(&key).into_string());
                        let secret_id = SecretsId::new(app_key.clone().into_bytes());
                        
                        // Update the key index
                        let mut outbound_msgs = Vec::new();
                        
                        // First, get the current key index
                        let index_key = format!("{}:{}", app_id, KEY_INDEX_SUFFIX);
                        let index_secret_id = SecretsId::new(index_key.clone().into_bytes());
                        
                        // Request the current index
                        let get_index = OutboundDelegateMsg::GetSecretRequest(
                            GetSecretRequest {
                                key: index_secret_id.clone(),
                                context: DelegateContext::default(),
                                processed: false,
                            }
                        );
                        
                        // Store the original request in context for later processing after we get the index
                        context.pending_gets.insert(index_key.clone(), (app_msg.app, key.clone()));
                        
                        // Create response
                        let response = ChatDelegateResponseMsg::DeleteResponse {
                            key,
                            result: Ok(()),
                        };
                        
                        // Serialize response
                        let mut response_bytes = Vec::new();
                        ciborium::ser::into_writer(&response, &mut response_bytes)
                            .map_err(|e| DelegateError::Deser(format!("Failed to serialize response: {e}")))?;
                        
                        // Serialize context
                        let mut context_bytes = Vec::new();
                        ciborium::ser::into_writer(&context, &mut context_bytes)
                            .map_err(|e| DelegateError::Deser(format!("Failed to serialize context: {e}")))?;
                        
                        // Create response message and set secret request
                        let app_response = OutboundDelegateMsg::ApplicationMessage(
                            ApplicationMessage::new(app_msg.app, response_bytes)
                                .with_context(DelegateContext::new(context_bytes))
                                .processed(true)
                        );
                        
                        let set_secret = OutboundDelegateMsg::SetSecretRequest(
                            SetSecretRequest {
                                key: secret_id,
                                value: None, // Setting to None deletes the secret
                            }
                        );
                        
                        // Update context in the get_index request
                        let mut get_index_msgs = vec![get_index];
                        if let Some(ctx) = get_index_msgs[0].get_mut_context() {
                            ctx.replace(context_bytes.clone());
                        }
                        
                        // We'll first get the index, then update it in the GetSecretResponse handler
                        outbound_msgs.push(set_secret);
                        outbound_msgs.push(get_index_msgs.remove(0));
                        
                        Ok(outbound_msgs)
                    },

                    ChatDelegateRequestMsg::ListRequest => {
                        // Get the key index for this app
                        let index_key = format!("{}:{}", app_id, KEY_INDEX_SUFFIX);
                        let index_secret_id = SecretsId::new(index_key.clone().into_bytes());
                        
                        // Request the current index
                        let get_index = OutboundDelegateMsg::GetSecretRequest(
                            GetSecretRequest {
                                key: index_secret_id,
                                context: DelegateContext::default(),
                                processed: false,
                            }
                        );
                        
                        // Store a special marker in the context to indicate this is a list request
                        context.pending_gets.insert(index_key, (app_msg.app, Vec::new()));
                        
                        // Serialize context
                        let mut context_bytes = Vec::new();
                        ciborium::ser::into_writer(&context, &mut context_bytes)
                            .map_err(|e| DelegateError::Deser(format!("Failed to serialize context: {e}")))?;
                        
                        // Update context in the get_index request
                        let mut get_index_msgs = vec![get_index];
                        if let Some(ctx) = get_index_msgs[0].get_mut_context() {
                            ctx.replace(context_bytes);
                        }
                        
                        Ok(get_index_msgs)
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
                    // This is a response to a key index request
                    if let Some((app_id, original_key)) = context.pending_gets.get(&app_key).cloned() {
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
                            
                            // Serialize response
                            let mut response_bytes = Vec::new();
                            ciborium::ser::into_writer(&response, &mut response_bytes)
                                .map_err(|e| DelegateError::Deser(format!("Failed to serialize response: {e}")))?;
                            
                            // Create response message
                            let app_response = OutboundDelegateMsg::ApplicationMessage(
                                ApplicationMessage::new(app_id, response_bytes)
                                    .with_context(DelegateContext::default())
                                    .processed(true)
                            );
                            
                            outbound_msgs.push(app_response);
                        } else {
                            // This is a store or delete operation that needs to update the index
                            
                            // For store operations, add the key if it doesn't exist
                            if !original_key.is_empty() && !key_index.keys.contains(&original_key) {
                                key_index.keys.push(original_key.clone());
                            }
                            
                            // For delete operations, remove the key
                            if context.pending_gets.contains_key(&app_key) && 
                               app_key.split(':').nth(1).map_or(false, |k| k.starts_with(bs58::encode(&original_key).into_string().as_str())) {
                                key_index.keys.retain(|k| k != &original_key);
                            }
                            
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
                            
                            // Create app response for the original operation
                            // This is already handled by the original operation, so we don't need to do it here
                        }
                        
                        // Remove the pending get request
                        context.pending_gets.remove(&app_key);
                        
                        return Ok(outbound_msgs);
                    } else {
                        // No pending get request for this key index
                        return Err(DelegateError::Other(format!("No pending key index request for: {app_key}")));
                    }
                } else if let Some((app_id, original_key)) = context.pending_gets.get(&app_key) {
                    // This is a regular get request
                    
                    // Create response
                    let response = ChatDelegateResponseMsg::GetResponse {
                        key: original_key.clone(),
                        value: get_secret_response.value,
                    };
                    
                    // Serialize response
                    let mut response_bytes = Vec::new();
                    ciborium::ser::into_writer(&response, &mut response_bytes)
                        .map_err(|e| DelegateError::Deser(format!("Failed to serialize response: {e}")))?;
                    
                    // Create response message
                    let app_response = OutboundDelegateMsg::ApplicationMessage(
                        ApplicationMessage::new(*app_id, response_bytes)
                            .with_context(DelegateContext::default())
                            .processed(true)
                    );
                    
                    // Remove the pending get request
                    context.pending_gets.remove(&app_key);
                    
                    Ok(vec![app_response])
                } else {
                    Err(DelegateError::Other(format!("No pending get request for key: {app_key}")))
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
struct Context {
    // Map of app-specific keys to (app_id, original_key) for pending get requests
    pending_gets: HashMap<String, (freenet_stdlib::prelude::ContractInstanceId, Vec<u8>)>,
    // We don't need to store rooms in the context anymore as we use the secret storage
}

// Structure to store the index of keys for an app
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct KeyIndex {
    keys: Vec<Vec<u8>>,
}

// Constants for key index
const KEY_INDEX_SUFFIX: &str = "::key_index";

// Helper function to deserialize context or create a default one
fn deserialize_context(context_bytes: &[u8]) -> Result<Context, DelegateError> {
    if context_bytes.is_empty() {
        Ok(Context::default())
    } else {
        ciborium::from_reader(context_bytes)
            .map_err(|e| DelegateError::Deser(format!("Failed to deserialize context: {e}")))
    }
}
