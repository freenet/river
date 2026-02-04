use super::*;

/// Structure to store the index of keys for an origin
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct KeyIndex {
    pub(super) keys: Vec<ChatDelegateKey>,
}
