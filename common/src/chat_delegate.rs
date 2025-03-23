use serde::{Deserialize, Serialize};

/// Messages sent from the App to the Chat Delegate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateRequestMsg {
    StoreRequest { key: Vec<u8>, value: Vec<u8> },
    GetRequest { key: Vec<u8> },
    DeleteRequest { key: Vec<u8> },
    ListRequest,
}

/// Responses sent from the Chat Delegate to the App
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateResponseMsg {
    GetResponse {
        key: Vec<u8>,
        value: Option<Vec<u8>>,
    },
    ListResponse {
        keys: Vec<Vec<u8>>,
    },
    StoreResponse {
        key: Vec<u8>,
        value_size: usize,
        result: Result<(), String>,
    },
    DeleteResponse {
        key: Vec<u8>,
        result: Result<(), String>,
    },
}
