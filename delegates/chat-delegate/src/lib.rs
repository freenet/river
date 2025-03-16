use std::collections::HashMap;
use freenet_stdlib::prelude::{
    delegate, ApplicationMessage, DelegateContext, DelegateError, DelegateInterface, GetSecretRequest,
    InboundDelegateMsg, OutboundDelegateMsg, Parameters, SecretsId, SetSecretRequest,
};
use serde::{Deserialize, Serialize};

// Define our own message types since we don't have access to river-common
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateRequestMsg {
    StoreRequest {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    GetRequest {
        key: Vec<u8>,
    },
    DeleteRequest {
        key: Vec<u8>,
    },
    ListRequest {
        key_prefix: Vec<u8>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateResponseMsg {
    StoreResponse {
        key: Vec<u8>,
        result: Result<(), String>,
    },
    GetResponse {
        key: Vec<u8>,
        value: Option<Vec<u8>>,
    },
    DeleteResponse {
        key: Vec<u8>,
        result: Result<(), String>,
    },
    ListResponse {
        key_prefix: Vec<u8>,
        keys: Vec<Vec<u8>>,
    },
}

pub struct RoomDelegate;

#[delegate]
impl DelegateInterface for RoomDelegate {
    fn process(
        _parameters: Parameters<'static>,
        attested: Option<&'static [u8]>,
        message: InboundDelegateMsg
    ) -> Result<Vec<OutboundDelegateMsg>, DelegateError> {
        match message {
            InboundDelegateMsg::ApplicationMessage(app_msg) => {
                let mut context: Context = if app_msg.context.as_ref().is_empty() {
                    Context::default()
                } else {
                    ciborium::from_reader(app_msg.context.as_ref())
                        .map_err(|e| DelegateError::Deser(format!("Failed to deserialize context: {e}")))?
                };

                // Deserialize the request message
                let request: ChatDelegateRequestMsg = ciborium::from_reader(app_msg.payload.as_slice())
                    .map_err(|e| DelegateError::Deser(format!("Failed to deserialize request: {e}")))?;

                // Create app-specific key prefix
                let app_id = app_msg.app.to_string();
                
                match request {
                    ChatDelegateRequestMsg::StoreRequest { key, value } => {
                        // Create a unique key for this app's data
                        let app_key = format!("{}:{}", app_id, bs58::encode(&key).into_string());
                        let secret_id = SecretsId::new(app_key.into_bytes());
                        
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
                                value: Some(value),
                            }
                        );
                        
                        Ok(vec![app_response, set_secret])
                    },
                    
                    ChatDelegateRequestMsg::GetRequest { key } => {
                        // Create a unique key for this app's data
                        let app_key = format!("{}:{}", app_id, bs58::encode(&key).into_string());
                        let secret_id = SecretsId::new(app_key.into_bytes());
                        
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
                        let secret_id = SecretsId::new(app_key.into_bytes());
                        
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
                        
                        Ok(vec![app_response, set_secret])
                    },
                    
                    ChatDelegateRequestMsg::ListRequest { key_prefix } => {
                        // This is a more complex operation that would require listing all secrets
                        // and filtering by app ID and prefix. For now, we'll return an empty list.
                        let response = ChatDelegateResponseMsg::ListResponse {
                            key_prefix,
                            keys: Vec::new(),
                        };
                        
                        // Serialize response
                        let mut response_bytes = Vec::new();
                        ciborium::ser::into_writer(&response, &mut response_bytes)
                            .map_err(|e| DelegateError::Deser(format!("Failed to serialize response: {e}")))?;
                        
                        // Serialize context
                        let mut context_bytes = Vec::new();
                        ciborium::ser::into_writer(&context, &mut context_bytes)
                            .map_err(|e| DelegateError::Deser(format!("Failed to serialize context: {e}")))?;
                        
                        // Create response message
                        let app_response = OutboundDelegateMsg::ApplicationMessage(
                            ApplicationMessage::new(app_msg.app, response_bytes)
                                .with_context(DelegateContext::new(context_bytes))
                                .processed(true)
                        );
                        
                        Ok(vec![app_response])
                    },
                }
            },
            
            InboundDelegateMsg::GetSecretResponse(get_secret_response) => {
                // Deserialize context
                let context: Context = if get_secret_response.context.as_ref().is_empty() {
                    Context::default()
                } else {
                    ciborium::from_reader(get_secret_response.context.as_ref())
                        .map_err(|e| DelegateError::Deser(format!("Failed to deserialize context: {e}")))?
                };
                
                // Get the app_key from the secret ID
                let app_key = String::from_utf8(get_secret_response.key.key().to_vec())
                    .map_err(|e| DelegateError::Other(format!("Invalid UTF-8 in key: {e}")))?;
                
                // Look up the pending get request
                if let Some((app_id, original_key)) = context.pending_gets.get(&app_key) {
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
