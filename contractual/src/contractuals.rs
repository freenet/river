use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use crate::Contractual;

#[derive(Clone)]
pub struct Signed<T>
where
    T: Serialize + Clone,
{
    pub message: T,
    pub signature: Signature,
}

impl<T> Signed<T>
where
    T: Serialize + Clone,
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

impl<T> Serialize for Signed<T>
where
    T: Serialize + Clone,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("Signed", 2)?;
        state.serialize_field("message", &self.message)?;
        state.serialize_field("signature", &self.signature.to_bytes())?;
        state.end()
    }
}

impl<'de, T> Deserialize<'de> for Signed<T>
where
    T: Serialize + Deserialize<'de> + Clone,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct SignedHelper<T> {
            message: T,
            signature: [u8; 64],
        }

        let helper = SignedHelper::deserialize(deserializer)?;
        Ok(Signed {
            message: helper.message,
            signature: Signature::from_bytes(&helper.signature).map_err(serde::de::Error::custom)?,
        })
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

        let message = String::from("Hello, World!");
        let signed = Signed::new(message, &signing_key).unwrap();
        assert!(signed.verify(&verifying_key).is_ok());
    }
}
