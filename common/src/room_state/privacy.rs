use serde::{Deserialize, Serialize};
use std::fmt;

/// Version identifier for room secrets
pub type SecretVersion = u32;

/// Privacy mode for a chat room
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub enum PrivacyMode {
    /// Room content is visible to all network participants
    #[default]
    Public,
    /// Room content is encrypted and only visible to members
    Private,
}

/// Cipher specification for encrypted room content
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum RoomCipherSpec {
    /// AES-256-GCM with 12-byte nonce
    Aes256Gcm,
}

/// A value that may be public or encrypted
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum SealedBytes {
    /// Plaintext value (only for public rooms)
    Public { value: Vec<u8> },
    /// Encrypted value with metadata
    Private {
        ciphertext: Vec<u8>,
        nonce: [u8; 12],
        secret_version: SecretVersion,
        declared_len_bytes: u32,
    },
}

impl SealedBytes {
    /// Create a new public sealed bytes value
    pub fn public(value: Vec<u8>) -> Self {
        Self::Public { value }
    }

    /// Create a new private sealed bytes value
    pub fn private(
        ciphertext: Vec<u8>,
        nonce: [u8; 12],
        secret_version: SecretVersion,
        declared_len_bytes: u32,
    ) -> Self {
        Self::Private {
            ciphertext,
            nonce,
            secret_version,
            declared_len_bytes,
        }
    }

    /// Check if this is a public value
    pub fn is_public(&self) -> bool {
        matches!(self, Self::Public { .. })
    }

    /// Check if this is a private value
    pub fn is_private(&self) -> bool {
        matches!(self, Self::Private { .. })
    }

    /// Get the declared length in bytes for validation
    pub fn declared_len(&self) -> usize {
        match self {
            Self::Public { value } => value.len(),
            Self::Private {
                declared_len_bytes, ..
            } => *declared_len_bytes as usize,
        }
    }

    /// Get the secret version (if private)
    pub fn secret_version(&self) -> Option<SecretVersion> {
        match self {
            Self::Public { .. } => None,
            Self::Private { secret_version, .. } => Some(*secret_version),
        }
    }

    /// Get the value if public, otherwise return a placeholder
    /// This is a temporary helper for UI integration during development
    pub fn to_string_lossy(&self) -> String {
        match self {
            Self::Public { value } => String::from_utf8_lossy(value).to_string(),
            Self::Private {
                declared_len_bytes,
                secret_version,
                ..
            } => {
                format!(
                    "[Encrypted: {} bytes, v{}]",
                    declared_len_bytes, secret_version
                )
            }
        }
    }

    /// Try to get the public value as bytes, returns None if private
    pub fn as_public_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Public { value } => Some(value),
            Self::Private { .. } => None,
        }
    }
}

impl fmt::Display for SealedBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_string_lossy())
    }
}

/// Display metadata for a room (name and optional description)
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct RoomDisplayMetadata {
    pub name: SealedBytes,
    pub description: Option<SealedBytes>,
}

impl RoomDisplayMetadata {
    /// Create public display metadata
    pub fn public(name: String, description: Option<String>) -> Self {
        Self {
            name: SealedBytes::public(name.into_bytes()),
            description: description.map(|d| SealedBytes::public(d.into_bytes())),
        }
    }

    /// Create private display metadata
    pub fn private(
        name_ciphertext: Vec<u8>,
        name_nonce: [u8; 12],
        name_declared_len: u32,
        description: Option<(Vec<u8>, [u8; 12], u32)>,
        secret_version: SecretVersion,
    ) -> Self {
        Self {
            name: SealedBytes::private(
                name_ciphertext,
                name_nonce,
                secret_version,
                name_declared_len,
            ),
            description: description.map(|(ciphertext, nonce, declared_len)| {
                SealedBytes::private(ciphertext, nonce, secret_version, declared_len)
            }),
        }
    }

    /// Check if both name and description are public
    pub fn is_public(&self) -> bool {
        self.name.is_public() && self.description.as_ref().is_none_or(|d| d.is_public())
    }

    /// Check if name is private
    pub fn is_private(&self) -> bool {
        self.name.is_private()
    }
}

impl Default for RoomDisplayMetadata {
    fn default() -> Self {
        Self::public("Default Room Name".to_string(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_privacy_mode_default() {
        assert_eq!(PrivacyMode::default(), PrivacyMode::Public);
    }

    #[test]
    fn test_sealed_bytes_public() {
        let data = b"test data".to_vec();
        let sealed = SealedBytes::public(data.clone());

        assert!(sealed.is_public());
        assert!(!sealed.is_private());
        assert_eq!(sealed.declared_len(), data.len());
        assert_eq!(sealed.secret_version(), None);
    }

    #[test]
    fn test_sealed_bytes_private() {
        let ciphertext = vec![1, 2, 3, 4];
        let nonce = [0u8; 12];
        let secret_version = 1;
        let declared_len = 10;

        let sealed = SealedBytes::private(ciphertext.clone(), nonce, secret_version, declared_len);

        assert!(!sealed.is_public());
        assert!(sealed.is_private());
        assert_eq!(sealed.declared_len(), declared_len as usize);
        assert_eq!(sealed.secret_version(), Some(secret_version));
    }
}
