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
    /// the error arm can tell WHICH delegate is missing (freenet/river#707): the
    /// current one (register it) or a legacy one (the migration journal's "not
    /// installed" verdict). Displays exactly like the `WebSocketError` it used
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

    /// The subscribe request could not be sent. Displays (and so logs) like
    /// `SubscribeError`; `summary` is the short explanation the room's error
    /// report shows, `detail` the redacted text its copied diagnostics carry
    /// ([`ErrorReport`](super::error_report::ErrorReport)).
    #[error("Failed to subscribe to contract: {raw}")]
    SubscribeSendFailed {
        summary: String,
        raw: String,
        detail: String,
    },

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

    /// One short sentence for the user. Exhaustive, so a new variant has to
    /// say what it means to someone who is not reading the logs; the Display
    /// above stays the log text.
    pub fn user_message(&self) -> String {
        use super::error_report::first_sentence;
        match self {
            SynchronizerError::WebSocketError(m)
            | SynchronizerError::DelegateMissing { message: m, .. }
            | SynchronizerError::DelegateRegisterFailed { message: m, .. }
            | SynchronizerError::ClientApiError(m)
            | SynchronizerError::Unknown(m) => first_sentence(m),
            SynchronizerError::WebSocketNotSupported(_) => {
                "Your Freenet node does not support an operation River used".into()
            }
            SynchronizerError::ConnectionTimeout(ms) => {
                format!("The node did not answer within {} seconds", ms / 1000)
            }
            SynchronizerError::ApiNotInitialized => "Not connected to your Freenet node".into(),
            SynchronizerError::RoomNotFound(_) => "This room is not stored on this device".into(),
            SynchronizerError::ContractInfoNotFound(_) => {
                "River has no contract details for this room yet".into()
            }
            SynchronizerError::MessageSendError(m) => {
                format!("A message could not be sent: {}", first_sentence(m))
            }
            SynchronizerError::StateMergeError(_) | SynchronizerError::DeltaApplyError(_) => {
                "An update from the network could not be applied".into()
            }
            SynchronizerError::PutContractError(m) => {
                format!("The room could not be published: {}", first_sentence(m))
            }
            SynchronizerError::SubscribeError(m) => {
                format!("Couldn't subscribe to this room: {}", first_sentence(m))
            }
            SynchronizerError::SubscribeSendFailed { summary, .. } => {
                format!("Couldn't subscribe to this room: {summary}")
            }
            SynchronizerError::SerializationError(_) => "River could not encode a request".into(),
            SynchronizerError::DeserializationError(_) => {
                "River could not read a reply from the node".into()
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_send_failures_display_as_before_and_explain_shortly() {
        let e = SynchronizerError::SubscribeSendFailed {
            summary: "WebSocket state: CLOSED".into(),
            raw: "request error: {\"origin\":\"send precondition check\"}".into(),
            detail: "WebSocket error: request error: {}".into(),
        };
        assert_eq!(
            e.to_string(),
            SynchronizerError::SubscribeError(
                "request error: {\"origin\":\"send precondition check\"}".into()
            )
            .to_string()
        );
        assert_eq!(
            e.user_message(),
            "Couldn't subscribe to this room: WebSocket state: CLOSED"
        );
        assert_eq!(
            SynchronizerError::ApiNotInitialized.user_message(),
            "Not connected to your Freenet node"
        );
        assert_eq!(
            SynchronizerError::WebSocketError("connection closed. retrying".into()).user_message(),
            "Connection closed"
        );
    }
}
