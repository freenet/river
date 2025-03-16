use serde::{Deserialize, Serialize};

/// Messages sent from the App to the Chat Delegate
#[derive(Debug, Clone, Serialize, Deserialize)]
enum ChatDelegateRequestMsg {
    StoreRequest {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    GetRequest {
        key : Vec<u8>,
    },
    DeleteRequest {
        key : Vec<u8>,
    },
    ListRequest {
        key_prefix : Vec<u8>,
    },
}

/// Responses sent from the Chat Delegate to the App
#[derive(Debug, Clone, Serialize, Deserialize)]
enum ChatDelegateResponseMsg {
    GetResponse {
        key: Vec<u8>,
        value: Option<Vec<u8>>,
    },
    ListResponse {
        key_prefix: Vec<u8>,
        keys: Vec<(Vec<u8>)>,
    },
    StoreResponse {
        key: Vec<u8>,
        result: Result<(), String>,
    },
    DeleteResponse {
        key: Vec<u8>,
        result: Result<(), String>,
    },
}