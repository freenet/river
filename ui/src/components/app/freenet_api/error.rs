#![allow(dead_code)]

use freenet_stdlib::client_api;
use freenet_stdlib::client_api::{ClientError, DelegateError, ErrorKind, RequestError};
use freenet_stdlib::prelude::serde_json;
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
        SynchronizerError::ClientApiError(node_error_message(&error))
    }
}

/// A freenet-stdlib client error as one sentence: the browser client's JSON `error` field, not the whole payload.
pub fn node_error_message(error: &client_api::Error) -> String {
    #[cfg(target_family = "wasm")]
    {
        if let client_api::Error::ConnectionError(value) = error {
            if let Some(text) = connection_error_text(value) {
                return capitalise_first(text);
            }
        }
    }
    capitalise_first(&error.to_string())
}

fn connection_error_text(value: &serde_json::Value) -> Option<&str> {
    value.get("error")?.as_str().filter(|s| !s.is_empty())
}

fn capitalise_first(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
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

    /// The failure said once, for users; `Display` keeps its labels for logs and control flow.
    pub fn user_message(&self) -> String {
        match self {
            SynchronizerError::WebSocketError(msg)
            | SynchronizerError::WebSocketNotSupported(msg)
            | SynchronizerError::ClientApiError(msg)
            | SynchronizerError::Unknown(msg) => msg.clone(),
            SynchronizerError::DelegateMissing { message, .. }
            | SynchronizerError::DelegateRegisterFailed { message, .. } => message.clone(),
            SynchronizerError::ConnectionTimeout(_)
            | SynchronizerError::ApiNotInitialized
            | SynchronizerError::RoomNotFound(_)
            | SynchronizerError::ContractInfoNotFound(_)
            | SynchronizerError::MessageSendError(_)
            | SynchronizerError::StateMergeError(_)
            | SynchronizerError::DeltaApplyError(_)
            | SynchronizerError::PutContractError(_)
            | SynchronizerError::SubscribeError(_)
            | SynchronizerError::SerializationError(_)
            | SynchronizerError::DeserializationError(_) => self.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use freenet_stdlib::client_api;

    #[test]
    fn user_message_drops_a_label_that_only_restates_the_payload() {
        let msg = "WebSocket connection failed or timed out";
        for e in [
            SynchronizerError::WebSocketError(msg.into()),
            SynchronizerError::ClientApiError(msg.into()),
            SynchronizerError::Unknown(msg.into()),
        ] {
            assert_eq!(e.user_message(), msg);
        }
        // Display keeps the label: logs and the substring checks in freenet_synchronizer.rs read it.
        assert_eq!(
            SynchronizerError::WebSocketError(msg.into()).to_string(),
            format!("WebSocket connection error: {msg}")
        );
    }

    #[test]
    fn user_message_keeps_a_label_that_says_what_failed() {
        assert_eq!(
            SynchronizerError::SubscribeError("WebSocket is not open (state: CLOSED)".into())
                .user_message(),
            "Failed to subscribe to contract: WebSocket is not open (state: CLOSED)"
        );
        assert_eq!(
            SynchronizerError::ConnectionTimeout(5000).user_message(),
            "Connection timeout after 5000ms"
        );
    }

    #[test]
    fn connection_error_text_takes_only_the_error_field() {
        use freenet_stdlib::prelude::serde_json::json;
        assert_eq!(
            connection_error_text(&json!({"error": "connection closed", "source": "close"})),
            Some("connection closed")
        );
        assert_eq!(
            connection_error_text(&json!({
                "error": "WebSocket is not open (state: CLOSED)",
                "origin": "send precondition check",
                "request": "ContractOp(..)"
            })),
            Some("WebSocket is not open (state: CLOSED)")
        );
        for no_text in [
            json!({"source": "close"}),
            json!({"error": ""}),
            json!({"error": 5}),
        ] {
            assert_eq!(connection_error_text(&no_text), None, "{no_text}");
        }
    }

    #[test]
    fn a_client_error_becomes_one_capitalised_sentence() {
        assert_eq!(
            node_error_message(&client_api::Error::ConnectionClosed),
            "Connection closed"
        );
        assert_eq!(
            SynchronizerError::from(client_api::Error::ChannelClosed).user_message(),
            "Channel closed"
        );
    }
}
