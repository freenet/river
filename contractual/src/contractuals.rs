use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use crate::Contractual;

#[derive(Serialize, Deserialize, Clone)]
pub struct Signed<T, P, PS>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone,
    P: Serialize + for<'de> Deserialize<'de> + Clone,
    PS: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub message: T,
    pub signature: Signature,
    verifying_key_extractor: fn(&P, &PS, &T) -> VerifyingKey,
}

impl<T, P, PS> Signed<T, P, PS>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone,
    P: Serialize + for<'de> Deserialize<'de> + Clone,
    PS: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub fn new(message: T, signature: Signature, verifying_key_extractor: fn(&P, &PS, &T) -> VerifyingKey) -> Self {
        Self {
            message,
            signature,
            verifying_key_extractor,
        }
    }

    pub fn verify(&self, parameters: &P, parent_state: &PS) -> Result<(), String> {
        let verifying_key = (self.verifying_key_extractor)(parameters, parent_state, &self.message);
        let mut serialized_message = Vec::new();
        ciborium::ser::into_writer(&self.message, &mut serialized_message)
            .map_err(|e| format!("Serialization error: {}", e))?;
        
        verifying_key.verify(&serialized_message, &self.signature)
            .map_err(|e| format!("Signature verification failed: {}", e))
    }
}

impl<T, P, PS> Contractual for Signed<T, P, PS>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone,
    P: Serialize + for<'de> Deserialize<'de> + Clone,
    PS: Serialize + for<'de> Deserialize<'de> + Clone,
{
    type ParentState = PS;
    type Summary = Self;
    type Delta = Self;
    type Parameters = P;

    fn verify(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String> {
        self.verify(parameters, parent_state)
    }

    fn summarize(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters) -> Self::Summary {
        self.clone()
    }

    fn delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, _old_state_summary: &Self::Summary) -> Self::Delta {
        self.clone()
    }

    fn apply_delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, delta: &Self::Delta) -> Self {
        delta.clone()
    }
}
