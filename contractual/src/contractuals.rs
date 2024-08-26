use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use crate::Contractual;

#[derive(Clone)]
pub struct Signed<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone,
{
    pub message: T,
    pub signature: Signature,
}

impl<T> Serialize for Signed<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone,
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
    T: Serialize + for<'d> Deserialize<'d> + Clone,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, Visitor};
        use std::fmt;

        struct SignedVisitor<T>(std::marker::PhantomData<T>);

        impl<'de, T> Visitor<'de> for SignedVisitor<T>
        where
            T: Serialize + for<'d> Deserialize<'d> + Clone,
        {
            type Value = Signed<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct Signed")
            }

            fn visit_map<V>(self, mut map: V) -> Result<Signed<T>, V::Error>
            where
                V: de::MapAccess<'de>,
            {
                let mut message = None;
                let mut signature_bytes = None;
                while let Some(key) = map.next_key()? {
                    match key {
                        "message" => {
                            if message.is_some() {
                                return Err(de::Error::duplicate_field("message"));
                            }
                            message = Some(map.next_value()?);
                        }
                        "signature" => {
                            if signature_bytes.is_some() {
                                return Err(de::Error::duplicate_field("signature"));
                            }
                            signature_bytes = Some(map.next_value::<[u8; 64]>()?);
                        }
                        _ => {
                            return Err(de::Error::unknown_field(key, &["message", "signature"]));
                        }
                    }
                }
                let message = message.ok_or_else(|| de::Error::missing_field("message"))?;
                let signature_bytes = signature_bytes.ok_or_else(|| de::Error::missing_field("signature"))?;
                let signature = Signature::from_bytes(&signature_bytes).map_err(de::Error::custom)?;
                Ok(Signed { message, signature })
            }
        }

        deserializer.deserialize_struct("Signed", &["message", "signature"], SignedVisitor(std::marker::PhantomData))
    }
}

impl<T> Signed<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone,
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
    T: Serialize + for<'de> Deserialize<'de> + Clone,
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
        let signed = Signed::new(message, &signing_key).expect("Failed to create Signed");
        assert!(signed.verify(&verifying_key).is_ok());
    }
}
