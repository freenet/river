use super::*;

// Constants
pub(crate) const KEY_INDEX_SUFFIX: &str = "::key_index";
pub(crate) const ORIGIN_KEY_SEPARATOR: &str = ":";

/// Different types of pending operations
#[derive(Debug, Clone)]
pub(crate) enum PendingOperation {
    /// Regular get operation for a specific key
    Get {
        origin: Origin,
        client_key: ChatDelegateKey,
    },
    /// Store operation that needs to update the index
    Store {
        origin: Origin,
        client_key: ChatDelegateKey,
    },
    /// Delete operation that needs to update the index
    Delete {
        origin: Origin,
        client_key: ChatDelegateKey,
    },
    /// List operation to retrieve all keys
    List {
        origin: Origin,
    },
}

impl PendingOperation {
    pub(crate) fn is_delete_operation(&self) -> bool {
        matches!(self, Self::Delete { .. })
    }
}

/// Parameters for the chat delegate.
/// Currently empty, but could be extended with configuration options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChatDelegateParameters;

impl TryFrom<Parameters<'_>> for ChatDelegateParameters {
    type Error = DelegateError;

    fn try_from(_params: Parameters<'_>) -> Result<Self, Self::Error> {
        // Currently no parameters are used, but this could be extended
        Ok(Self {})
    }
}

/// A wrapper around SecretsId that implements Hash and Eq
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub(crate) struct SecretIdKey(pub(crate) String);

impl From<&SecretsId> for SecretIdKey {
    fn from(id: &SecretsId) -> Self {
        // Convert the SecretsId to a string representation for hashing
        Self(String::from_utf8_lossy(id.key()).to_string())
    }
}

// Add Serialize/Deserialize for PendingOperation
impl Serialize for PendingOperation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Get { origin, client_key } => {
                let mut seq = serializer.serialize_tuple(3)?;
                seq.serialize_element(&0u8)?; // Type tag for Get
                seq.serialize_element(origin)?;
                seq.serialize_element(client_key)?;
                seq.end()
            }
            Self::Store { origin, client_key } => {
                let mut seq = serializer.serialize_tuple(3)?;
                seq.serialize_element(&1u8)?; // Type tag for Store
                seq.serialize_element(origin)?;
                seq.serialize_element(client_key)?;
                seq.end()
            }
            Self::Delete { origin, client_key } => {
                let mut seq = serializer.serialize_tuple(3)?;
                seq.serialize_element(&2u8)?; // Type tag for Delete
                seq.serialize_element(origin)?;
                seq.serialize_element(client_key)?;
                seq.end()
            }
            Self::List { origin } => {
                let mut seq = serializer.serialize_tuple(2)?;
                seq.serialize_element(&3u8)?; // Type tag for List
                seq.serialize_element(origin)?;
                seq.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for PendingOperation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error, SeqAccess, Visitor};
        use std::fmt;

        struct PendingOpVisitor;

        impl<'de> Visitor<'de> for PendingOpVisitor {
            type Value = PendingOperation;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a tuple with a type tag and operation data")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let tag: u8 = seq.next_element()?.ok_or_else(|| Error::invalid_length(0, &self))?;
                
                match tag {
                    0 => { // Get
                        let origin: Origin = seq.next_element()?.ok_or_else(|| Error::invalid_length(1, &self))?;
                        let client_key: ChatDelegateKey = seq.next_element()?.ok_or_else(|| Error::invalid_length(2, &self))?;
                        Ok(PendingOperation::Get { origin, client_key })
                    },
                    1 => { // Store
                        let origin: Origin = seq.next_element()?.ok_or_else(|| Error::invalid_length(1, &self))?;
                        let client_key: ChatDelegateKey = seq.next_element()?.ok_or_else(|| Error::invalid_length(2, &self))?;
                        Ok(PendingOperation::Store { origin, client_key })
                    },
                    2 => { // Delete
                        let origin: Origin = seq.next_element()?.ok_or_else(|| Error::invalid_length(1, &self))?;
                        let client_key: ChatDelegateKey = seq.next_element()?.ok_or_else(|| Error::invalid_length(2, &self))?;
                        Ok(PendingOperation::Delete { origin, client_key })
                    },
                    3 => { // List
                        let origin: Origin = seq.next_element()?.ok_or_else(|| Error::invalid_length(1, &self))?;
                        Ok(PendingOperation::List { origin })
                    },
                    _ => Err(Error::custom(format!("Unknown operation type tag: {}", tag))),
                }
            }
        }

        deserializer.deserialize_tuple(3, PendingOpVisitor)
    }
}

/// Origin contract ID
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Origin(pub(crate) Vec<u8>);

impl Origin {
    pub(crate) fn to_b58(&self) -> String {
        bs58::encode(&self.0).into_string()
    }
}
