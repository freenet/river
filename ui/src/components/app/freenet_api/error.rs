#![allow(dead_code)]

use freenet_stdlib::client_api;
use freenet_stdlib::client_api::{ClientError, DelegateError, ErrorKind, RequestError};
use freenet_stdlib::prelude::DelegateKey;
use thiserror::Error;

/// Error types for the Freenet synchronizer
#[derive(Error, Debug, Clone)]
pub enum SynchronizerError {
    #[error("WebSocket connection error: {0}")]
    WebSocketError(String),

    /// The node's typed `DelegateError::Missing` for `key`. Kept structured so
    /// the legacy-migration seal can tell WHICH delegate is missing
    /// (freenet/river#707). Displays exactly like the `WebSocketError` it used
    /// to be mapped to.
    #[error("WebSocket connection error: {message}")]
    DelegateMissing { key: DelegateKey, message: String },

    /// The node's typed `DelegateError::RegisterError` for `key`, kept
    /// structured so the room load can stop waiting for the register reply
    /// (freenet/river#709). Displays like `WebSocketError`.
    #[error("WebSocket connection error: {message}")]
    DelegateRegisterFailed { key: DelegateKey, message: String },

    #[error("WebSocket operation not supported: {0}")]
    WebSocketNotSupported(String),

    #[error("Connection timeout after {0}ms")]
    ConnectionTimeout(u64),

    #[error("API not initialized")]
    ApiNotInitialized,

    #[error("Room data not found for key: {0}")]
    RoomNotFound(String),

    #[error("Contract info not found for key: {0}")]
    ContractInfoNotFound(String),

    #[error("Failed to send message: {0}")]
    MessageSendError(String),

    #[error("Failed to merge room state: {0}")]
    StateMergeError(String),

    #[error("Failed to apply delta to room state: {0}")]
    DeltaApplyError(String),

    #[error("Failed to put contract state: {0}")]
    PutContractError(String),

    #[error("Failed to subscribe to contract: {0}")]
    SubscribeError(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("Deserialization error: {0}")]
    DeserializationError(String),

    #[error("Client API error: {0}")]
    ClientApiError(String),

    #[error("Unknown error: {0}")]
    Unknown(String),
}

impl From<String> for SynchronizerError {
    fn from(error: String) -> Self {
        SynchronizerError::Unknown(error)
    }
}

impl From<&str> for SynchronizerError {
    fn from(error: &str) -> Self {
        SynchronizerError::Unknown(error.to_string())
    }
}

impl From<client_api::Error> for SynchronizerError {
    fn from(error: client_api::Error) -> Self {
        SynchronizerError::ClientApiError(error.to_string())
    }
}

impl SynchronizerError {
    /// Map an error the node sent in answer to a request.
    pub fn from_api_error(error: &ClientError) -> Self {
        match error.kind() {
            ErrorKind::RequestError(RequestError::DelegateError(DelegateError::Missing(key))) => {
                SynchronizerError::DelegateMissing {
                    key: key.clone(),
                    message: error.to_string(),
                }
            }
            ErrorKind::RequestError(RequestError::DelegateError(DelegateError::RegisterError(
                key,
            ))) => SynchronizerError::DelegateRegisterFailed {
                key: key.clone(),
                message: error.to_string(),
            },
            _ => SynchronizerError::WebSocketError(error.to_string()),
        }
    }

    /// The delegate a typed `DelegateError::Missing` names, if this is one.
    pub fn missing_delegate_key(&self) -> Option<&DelegateKey> {
        match self {
            SynchronizerError::DelegateMissing { key, .. } => Some(key),
            _ => None,
        }
    }

    /// The delegate a typed `DelegateError::RegisterError` names, if this is one.
    pub fn register_failed_key(&self) -> Option<&DelegateKey> {
        match self {
            SynchronizerError::DelegateRegisterFailed { key, .. } => Some(key),
            _ => None,
        }
    }
}
