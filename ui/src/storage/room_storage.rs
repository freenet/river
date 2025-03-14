use ed25519_dalek::SigningKey;
use river_common::crypto_values::CryptoValue;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct StoredRoomData {
    pub self_sk: String, // Store as encoded string for security/serialization
    // Future fields can be added here
}

impl StoredRoomData {
    pub fn new(signing_key: &SigningKey) -> Self {
        let encoded = CryptoValue::SigningKey(*signing_key).to_encoded_string();
        Self {
            self_sk: encoded,
        }
    }
    
    pub fn get_signing_key(&self) -> Result<SigningKey, String> {
        let crypto_value = CryptoValue::from_encoded_string(&self.self_sk)?;
        match crypto_value {
            CryptoValue::SigningKey(sk) => Ok(sk),
            _ => Err("Invalid crypto value type".to_string()),
        }
    }
}
