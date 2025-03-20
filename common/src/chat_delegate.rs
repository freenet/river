use serde::{Deserialize, Serialize};
use std::fmt;

/// Represents a key in the delegate storage
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct DelegateKey(pub Vec<u8>);

impl DelegateKey {
    pub fn new(key: Vec<u8>) -> Self {
        Self(key)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl fmt::Display for DelegateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", bs58::encode(&self.0).into_string())
    }
}

/// Represents a value in the delegate storage
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegateValue(pub Vec<u8>);

impl DelegateValue {
    pub fn new(value: Vec<u8>) -> Self {
        Self(value)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Helper to serialize a value of type T into a DelegateValue
    pub fn serialize<T: Serialize>(value: &T) -> Result<Self, String> {
        let mut buffer = Vec::new();
        ciborium::ser::into_writer(value, &mut buffer)
            .map_err(|e| format!("Serialization error: {}", e))?;
        Ok(Self(buffer))
    }

    /// Helper to deserialize a DelegateValue into a value of type T
    pub fn deserialize<T: for<'a> Deserialize<'a>>(&self) -> Result<T, String> {
        ciborium::de::from_reader(self.0.as_slice())
            .map_err(|e| format!("Deserialization error: {}", e))
    }
}

/// Messages sent from the App to the Chat Delegate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateRequestMsg {
    StoreRequest { key: DelegateKey, value: DelegateValue },
    GetRequest { key: DelegateKey },
    DeleteRequest { key: DelegateKey },
    ListRequest,
}

/// Responses sent from the Chat Delegate to the App
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateResponseMsg {
    GetResponse {
        key: DelegateKey,
        value: Option<DelegateValue>,
    },
    ListResponse {
        keys: Vec<DelegateKey>,
    },
    StoreResponse {
        key: DelegateKey,
        result: Result<(), String>,
    },
    DeleteResponse {
        key: DelegateKey,
        result: Result<(), String>,
    },
}
