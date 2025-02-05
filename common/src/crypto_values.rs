use ed25519_dalek::{SigningKey, VerifyingKey, Signature};
use std::str::FromStr;
use bs58;

#[derive(Debug, Clone, PartialEq)]
pub enum CryptoValue {
    VerifyingKey(VerifyingKey),
    SigningKey(SigningKey),
    Signature(Signature),
}

impl CryptoValue {
    const VERSION_PREFIX: &'static str = "river:v1";
    
    pub fn to_encoded_string(&self) -> String {
        let type_str = match self {
            CryptoValue::VerifyingKey(_) => "vk",
            CryptoValue::SigningKey(_) => "sk",
            CryptoValue::Signature(_) => "sig",
        };
        
        let key_bytes = match self {
            CryptoValue::VerifyingKey(vk) => vk.to_bytes().to_vec(),
            CryptoValue::SigningKey(sk) => sk.to_bytes().to_vec(),
            CryptoValue::Signature(sig) => sig.to_bytes().to_vec(),
        };
        
        format!(
            "{}:{}:{}",
            Self::VERSION_PREFIX,
            type_str,
            bs58::encode(key_bytes).into_string()
        )
    }
    
    pub fn from_encoded_string(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 4 || format!("{}:{}", parts[0], parts[1]) != Self::VERSION_PREFIX {
            return Err("Invalid format".to_string());
        }
        
        let decoded = bs58::decode(parts[3])
            .into_vec()
            .map_err(|e| format!("Base58 decode error: {}", e))?;
        
        match parts[2] {
            "vk" => {
                let bytes: [u8; 32] = decoded.try_into()
                    .map_err(|_| "Invalid verifying key length".to_string())?;
                VerifyingKey::from_bytes(&bytes)
                    .map(CryptoValue::VerifyingKey)
                    .map_err(|e| format!("Invalid verifying key: {}", e))
            },
            "sk" => {
                let bytes: [u8; 32] = decoded.try_into()
                    .map_err(|_| "Invalid signing key length".to_string())?;
                Ok(CryptoValue::SigningKey(SigningKey::from_bytes(&bytes)))
            },
            "sig" => {
                let bytes: [u8; 64] = decoded.try_into()
                    .map_err(|_| "Invalid signature length".to_string())?;
                Ok(CryptoValue::Signature(Signature::from_bytes(&bytes)))
            },
            _ => Err("Unknown key type".to_string()),
        }
    }
}

impl FromStr for CryptoValue {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // If string already contains prefix, use it directly
        if s.starts_with(Self::VERSION_PREFIX) {
            Self::from_encoded_string(s)
        } else {
            // Otherwise treat as raw base58 data
            let decoded = bs58::decode(s)
                .into_vec()
                .map_err(|e| format!("Base58 decode error: {}", e))?;
            
            // Try to interpret as signing key first
            if decoded.len() == 32 {
                let bytes: [u8; 32] = decoded.try_into()
                    .map_err(|_| "Invalid signing key length".to_string())?;
                Ok(CryptoValue::SigningKey(SigningKey::from_bytes(&bytes)))
            } else {
                Err("Invalid key length".to_string())
            }
        }
    }
}
