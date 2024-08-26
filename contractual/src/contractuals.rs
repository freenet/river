use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize};
use serde::de::DeserializeOwned;
use crate::Contractual;

#[derive(Serialize, Clone)]
pub struct Signed<T>
where
    T: Serialize + DeserializeOwned + Clone,
{
    pub message: T,
    pub signature: Signature,
}

impl<'de, T> Deserialize<'de> for Signed<T>
where
    T: Serialize + DeserializeOwned + Clone,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Deserialize the struct manually
        let (message, signature) = Deserialize::deserialize(deserializer)?;
        Ok(Signed { message, signature })
    }
}


impl<T> Signed<T>
where
    T: Serialize + DeserializeOwned + Clone,
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
