use ed25519_dalek::{SigningKey, VerifyingKey, Signature};
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq)]
pub enum CryptoKeyType {
    VerifyingKey(VerifyingKey),
    SigningKey(SigningKey),
    Signature(Signature),
}

impl CryptoKeyType {
    const VERSION_PREFIX: &'static str = "river:v1";
    
    pub fn to_encoded_string(&self) -> String {
        let type_str = match self {
            CryptoKeyType::VerifyingKey(_) => "vk",
            CryptoKeyType::SigningKey(_) => "sk",
            CryptoKeyType::Signature(_) => "sig",
        };
        
        let key_bytes = match self {
            CryptoKeyType::VerifyingKey(vk) => vk.to_bytes().to_vec(),
            CryptoKeyType::SigningKey(sk) => sk.to_bytes().to_vec(),
            CryptoKeyType::Signature(sig) => sig.to_bytes().to_vec(),
        };
        
        format!(
            "{}:{}:{}",
            Self::VERSION_PREFIX,
            type_str,
            base64::encode_config(key_bytes, base64::URL_SAFE)
        )
    }
    
    pub fn from_encoded_string(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 3 || parts[0] != Self::VERSION_PREFIX {
            return Err("Invalid format".to_string());
        }
        
        let decoded = base64::decode_config(parts[2], base64::URL_SAFE)
            .map_err(|e| format!("Base64 decode error: {}", e))?;
        
        match parts[1] {
            "vk" => VerifyingKey::from_bytes(&decoded)
                .map(CryptoKeyType::VerifyingKey)
                .map_err(|e| format!("Invalid verifying key: {}", e)),
            "sk" => SigningKey::from_bytes(&decoded)
                .map(CryptoKeyType::SigningKey)
                .map_err(|e| format!("Invalid signing key: {}", e)),
            "sig" => Signature::from_bytes(&decoded)
                .map(CryptoKeyType::Signature)
                .map_err(|e| format!("Invalid signature: {}", e)),
            _ => Err("Unknown key type".to_string()),
        }
    }
}

impl FromStr for CryptoKeyType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_encoded_string(s)
    }
}
