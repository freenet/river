use super::*;

/// Context for the chat delegate, storing pending operations.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct ChatDelegateContext {
    /// Map of secret IDs to pending operations
    pub(super) pending_ops: HashMap<SecretIdKey, PendingOperation>,
}

/// Structure to store the index of keys for an app
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(super) struct KeyIndex {
    pub(super) keys: Vec<ChatDelegateKey>,
}

impl TryFrom<DelegateContext> for ChatDelegateContext {
    type Error = DelegateError;

    fn try_from(value: DelegateContext) -> Result<Self, Self::Error> {
        if value == DelegateContext::default() {
            return Ok(Self::default());
        }
        ciborium::from_reader(value.as_ref())
            .map_err(|err| DelegateError::Deser(format!("Failed to deserialize context: {err}")))
    }
}

impl TryFrom<&ChatDelegateContext> for DelegateContext {
    type Error = DelegateError;

    fn try_from(value: &ChatDelegateContext) -> Result<Self, Self::Error> {
        let mut buffer = Vec::new();
        ciborium::ser::into_writer(value, &mut buffer)
            .map_err(|err| DelegateError::Deser(format!("Failed to serialize context: {err}")))?;
        Ok(DelegateContext::new(buffer))
    }
}

impl TryFrom<&mut ChatDelegateContext> for DelegateContext {
    type Error = DelegateError;

    fn try_from(value: &mut ChatDelegateContext) -> Result<Self, Self::Error> {
        // Delegate to the immutable reference implementation
        Self::try_from(&*value)
    }
}
