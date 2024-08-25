use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use crate::Contractual;

#[derive(Serialize, Deserialize, Clone)]
pub struct Signed<T>
where
    T: Serialize + Deserialize<'static> + Clone,
{
    pub message: T,
    pub signature: Signature,
}

impl<T> Signed<T>
where
    T: Serialize + Deserialize<'static> + Clone,
{
    pub fn new(message: T, signing_key: &SigningKey) -> Result<Self, String> {
        let mut serialized_message = Vec::new();
        ciborium::ser::into_writer(&message, &mut serialized_message)
            .map_err(|e| format!("Serialization error: {}", e))?;
        Ok(Self {
            message,
            signature: signing_key.sign(&serialized_message),
        })
    }

    pub fn verify(&self, verifying_key: &VerifyingKey) -> Result<(), String> {
        let mut serialized_message = Vec::new();
        ciborium::ser::into_writer(&self.message, &mut serialized_message)
            .map_err(|e| format!("Serialization error: {}", e))?;
        
        verifying_key.verify(&serialized_message, &self.signature)
            .map_err(|e| format!("Signature verification failed: {}", e))
    }
}

impl<T> Contractual for Signed<T>
where
    T: Serialize + Deserialize<'static> + Clone,
{
    type ParentState = ();
    type Summary = Self;
    type Delta = Self;
    type Parameters = VerifyingKey;

    fn verify(&self, _parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String> {
        self.verify(parameters)
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

// test
#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn test_signed() {
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        let verifying_key = signing_key.verifying_key();

        let message = "Hello, World!";
        let signed = Signed::new(message, &signing_key);
        assert!(signed.verify(&verifying_key).is_ok());
    }
}
