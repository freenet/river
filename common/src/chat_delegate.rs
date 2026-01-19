use serde::{Deserialize, Serialize};

/// Room key identifier (owner's verifying key bytes)
pub type RoomKey = [u8; 32];

/// Messages sent from the App to the Chat Delegate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateRequestMsg {
    // Key-value storage operations
    StoreRequest {
        key: ChatDelegateKey,
        value: Vec<u8>,
    },
    GetRequest {
        key: ChatDelegateKey,
    },
    DeleteRequest {
        key: ChatDelegateKey,
    },
    ListRequest,

    // Signing key management
    /// Store a signing key for a room (room_key = owner's verifying key bytes)
    StoreSigningKey {
        room_key: RoomKey,
        signing_key_bytes: [u8; 32],
    },
    /// Get the public key for a stored signing key
    GetPublicKey {
        room_key: RoomKey,
    },

    // Signing operations - pass serialized data, get signature back
    /// Sign a message (MessageV1 serialized)
    SignMessage {
        room_key: RoomKey,
        message_bytes: Vec<u8>,
    },
    /// Sign a member invitation (Member serialized)
    SignMember {
        room_key: RoomKey,
        member_bytes: Vec<u8>,
    },
    /// Sign a ban (BanV1 serialized)
    SignBan {
        room_key: RoomKey,
        ban_bytes: Vec<u8>,
    },
    /// Sign a room configuration (Configuration serialized)
    SignConfig {
        room_key: RoomKey,
        config_bytes: Vec<u8>,
    },
    /// Sign member info (MemberInfo serialized)
    SignMemberInfo {
        room_key: RoomKey,
        member_info_bytes: Vec<u8>,
    },
    /// Sign a secret version record (SecretVersionRecordV1 serialized)
    SignSecretVersion {
        room_key: RoomKey,
        record_bytes: Vec<u8>,
    },
    /// Sign an encrypted secret for member (EncryptedSecretForMemberV1 serialized)
    SignEncryptedSecret {
        room_key: RoomKey,
        secret_bytes: Vec<u8>,
    },
    /// Sign a room upgrade (RoomUpgrade serialized)
    SignUpgrade {
        room_key: RoomKey,
        upgrade_bytes: Vec<u8>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ChatDelegateKey(pub Vec<u8>);

impl ChatDelegateKey {
    pub fn new(key: Vec<u8>) -> Self {
        Self(key)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Responses sent from the Chat Delegate to the App
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateResponseMsg {
    // Key-value storage responses
    GetResponse {
        key: ChatDelegateKey,
        value: Option<Vec<u8>>,
    },
    ListResponse {
        keys: Vec<ChatDelegateKey>,
    },
    StoreResponse {
        key: ChatDelegateKey,
        value_size: usize,
        result: Result<(), String>,
    },
    DeleteResponse {
        key: ChatDelegateKey,
        result: Result<(), String>,
    },

    // Signing key management responses
    /// Response to StoreSigningKey
    StoreSigningKeyResponse {
        room_key: RoomKey,
        result: Result<(), String>,
    },
    /// Response to GetPublicKey
    GetPublicKeyResponse {
        room_key: RoomKey,
        /// The public key bytes if the signing key exists
        public_key: Option<[u8; 32]>,
    },

    // Signing response (used for all signing operations)
    /// Response to any signing operation
    SignResponse {
        room_key: RoomKey,
        /// The signature bytes (64 bytes for Ed25519, as Vec for serde compatibility)
        signature: Result<Vec<u8>, String>,
    },
}
