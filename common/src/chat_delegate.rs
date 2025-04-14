use serde::{Deserialize, Serialize};

/// Messages sent from the App to the Chat Delegate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateRequestMsg {
    StoreRequest {
        key: ChatDelegateKey,
        value: Vec<u8>,
    },
    GetRequest {
        key: ChatDelegateKey,
    },
    DeleteRequest {
        key: ChatDelegateKey,
    },
    ListRequest,
}

#[cfg(feature = "disable-delegates")]
impl ChatDelegateRequestMsg {
    /// Creates a no-op response for the request when delegates are disabled
    pub fn create_no_op_response(&self) -> ChatDelegateResponseMsg {
        match self {
            Self::StoreRequest { key, value } => ChatDelegateResponseMsg::StoreResponse {
                key: key.clone(),
                value_size: value.len(),
                result: Ok(()),
            },
            Self::GetRequest { key } => ChatDelegateResponseMsg::GetResponse {
                key: key.clone(),
                value: None,
            },
            Self::DeleteRequest { key } => ChatDelegateResponseMsg::DeleteResponse {
                key: key.clone(),
                result: Ok(()),
            },
            Self::ListRequest => ChatDelegateResponseMsg::ListResponse { keys: vec![] },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ChatDelegateKey(pub Vec<u8>);

impl ChatDelegateKey {
    pub fn new(key: Vec<u8>) -> Self {
        Self(key)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Responses sent from the Chat Delegate to the App
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateResponseMsg {
    GetResponse {
        key: ChatDelegateKey,
        value: Option<Vec<u8>>,
    },
    ListResponse {
        keys: Vec<ChatDelegateKey>,
    },
    StoreResponse {
        key: ChatDelegateKey,
        value_size: usize,
        result: Result<(), String>,
    },
    DeleteResponse {
        key: ChatDelegateKey,
        result: Result<(), String>,
    },
}
