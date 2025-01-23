use ed25519_dalek::{SigningKey, VerifyingKey, Signature};
use std::str::FromStr;
use base64::{Engine as _, engine::general_purpose::URL_SAFE};

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
            URL_SAFE.encode(key_bytes)
        )
    }
    
    pub fn from_encoded_string(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 3 || parts[0] != Self::VERSION_PREFIX {
            return Err("Invalid format".to_string());
        }
        
        let decoded = URL_SAFE.decode(parts[2])
            .map_err(|e| format!("Base64 decode error: {}", e))?;
        
        match parts[1] {
            "vk" => VerifyingKey::from_bytes(&decoded)
                .map(CryptoKeyType::VerifyingKey)
                .map_err(|e| format!("Invalid verifying key: {}", e)),
            "sk" => Ok(CryptoKeyType::SigningKey(
                SigningKey::from_bytes(&decoded.try_into().map_err(|_| "Invalid key length")?)
            )),
            "sig" => Ok(CryptoKeyType::Signature(
                Signature::from_bytes(&decoded.try_into().map_err(|_| "Invalid signature length")?)
            )),
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
