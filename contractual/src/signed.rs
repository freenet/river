use std::io::Cursor;
use ed25519_dalek::{Signature, SignatureError, Signer, SigningKey, Verifier, VerifyingKey};
use serde::de::DeserializeOwned;
use serde::Serialize;

pub struct Signed<T> {
    pub data: Vec<u8>,
    pub signature: Signature,
    pub _phantom: std::marker::PhantomData<T>,
}

impl<T : Serialize + DeserializeOwned> Signed<T> {
    pub fn new(data: T, signing_key: &SigningKey) -> Self {
        let mut data_to_sign = Vec::new();
        ciborium::ser::into_writer(&data, &mut data_to_sign).expect("Serialization should not fail");
        let signature = signing_key.sign(&data_to_sign);
        Self {
            data: data_to_sign,
            signature,
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn verify(&self, verifying_key: &VerifyingKey) -> Result<T, SignatureError> {
        verifying_key.verify(&self.data, &self.signature)?;
        let cursor = Cursor::new(&self.data);
        Ok(ciborium::de::from_reader(cursor).expect("Deserialization should not fail"))
    }
}

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
        let signed = Signed::new(message.to_string(), &signing_key);
        assert_eq!(signed.verify(&verifying_key).unwrap(), message);
    }
}