use serde::{Deserialize, Serialize};

use crate::room_state::direct_messages::PurgeToken;
use crate::room_state::member::MemberId;

/// Room key identifier (owner's verifying key bytes)
pub type RoomKey = [u8; 32];

/// Delegate storage key for the outbound-DM plaintext cache.
///
/// Lets the sender re-render their own DMs as plaintext on reload /
/// secondary device, since the room contract only carries
/// ECIES-ciphertext (only the recipient can decrypt). See issue
/// freenet/river#256.
pub const OUTBOUND_DMS_STORAGE_KEY: &[u8] = b"outbound_dms";

/// Persistent cache of outbound DM plaintext, keyed by
/// `(room_owner_vk, recipient, purge_token)` inside each entry.
///
/// Stored as a `Vec` rather than `HashMap` so JSON serialization works
/// — see the "non-string map keys" bug-prevention pattern in
/// `freenet/.claude/rules/bug-prevention-patterns.md`. Lookups are
/// linear, which is fine: the store is bounded by per-pair caps
/// (`MAX_DM_MESSAGES_PER_PAIR`) and pruned on purge tombstones.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutboundDmStore {
    #[serde(default)]
    pub entries: Vec<OutboundDmEntry>,
}

/// A single outbound DM the local user composed and sent.
///
/// `purge_token` matches `AuthorizedDirectMessage::purge_token()` for
/// the ciphertext that was emitted, so the UI/CLI can join the local
/// plaintext to the contract-state ciphertext entry, and so that
/// purge tombstones (which list `PurgeToken`s) can prune this store in
/// lockstep with the contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutboundDmEntry {
    /// Room owner verifying key — disambiguates the same recipient
    /// being a member of multiple rooms. Raw 32 bytes to match the
    /// `RoomKey` convention used elsewhere in this module and to keep
    /// the type JSON-friendly.
    pub room_owner_vk: [u8; 32],
    /// Local user's `MemberId` *at send time*, derived from the room
    /// signing key. Present so a second device that re-loads under a
    /// different room identity can tell which of its identities sent
    /// the DM.
    pub sender: MemberId,
    pub recipient: MemberId,
    pub purge_token: PurgeToken,
    /// Unix seconds — same value used in the on-wire `DirectMessage`.
    pub timestamp: u64,
    pub plaintext: String,
}

/// Unique identifier for a signing request (for request/response correlation)
pub type RequestId = u64;

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
    // All signing ops include request_id for response correlation
    /// Sign a message (MessageV1 serialized)
    SignMessage {
        room_key: RoomKey,
        request_id: RequestId,
        message_bytes: Vec<u8>,
    },
    /// Sign a member invitation (Member serialized)
    SignMember {
        room_key: RoomKey,
        request_id: RequestId,
        member_bytes: Vec<u8>,
    },
    /// Sign a ban (BanV1 serialized)
    SignBan {
        room_key: RoomKey,
        request_id: RequestId,
        ban_bytes: Vec<u8>,
    },
    /// Sign a room configuration (Configuration serialized)
    SignConfig {
        room_key: RoomKey,
        request_id: RequestId,
        config_bytes: Vec<u8>,
    },
    /// Sign member info (MemberInfo serialized)
    SignMemberInfo {
        room_key: RoomKey,
        request_id: RequestId,
        member_info_bytes: Vec<u8>,
    },
    /// Sign a secret version record (SecretVersionRecordV1 serialized)
    SignSecretVersion {
        room_key: RoomKey,
        request_id: RequestId,
        record_bytes: Vec<u8>,
    },
    /// Sign an encrypted secret for member (EncryptedSecretForMemberV1 serialized)
    SignEncryptedSecret {
        room_key: RoomKey,
        request_id: RequestId,
        secret_bytes: Vec<u8>,
    },
    /// Sign a room upgrade (RoomUpgrade serialized)
    SignUpgrade {
        room_key: RoomKey,
        request_id: RequestId,
        upgrade_bytes: Vec<u8>,
    },

    /// Ask the delegate to subscribe to a room contract so the delegate can
    /// drive secret rotation when the membership set changes.
    ///
    /// `contract_id` is the 32-byte ContractInstanceId for the room contract,
    /// computed by the UI as `BLAKE3(room_contract_wasm_hash || params)` where
    /// `params` is the cbor-serialised `ChatRoomParametersV1 { owner: room_owner_vk }`.
    /// We pass it explicitly rather than recomputing it inside the delegate so
    /// that the delegate WASM doesn't need to bundle the room-contract WASM.
    EnsureRoomSubscription {
        room_owner_vk: RoomKey,
        contract_id: [u8; 32],
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
        /// The request ID for correlation
        request_id: RequestId,
        /// The signature bytes (64 bytes for Ed25519, as Vec for serde compatibility)
        signature: Result<Vec<u8>, String>,
    },

    /// Response to [`ChatDelegateRequestMsg::EnsureRoomSubscription`].
    ///
    /// `Ok(())` means the delegate emitted a `SubscribeContractRequest` to the
    /// runtime; the actual subscription confirmation flows back to the
    /// delegate as `InboundDelegateMsg::SubscribeContractResponse` and is not
    /// surfaced to the UI.
    EnsureRoomSubscriptionResponse {
        room_owner_vk: RoomKey,
        result: Result<(), String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use freenet_scaffold::util::FastHash;

    fn sample_entry() -> OutboundDmEntry {
        OutboundDmEntry {
            room_owner_vk: [9u8; 32],
            sender: MemberId(FastHash(0xdead_beef)),
            recipient: MemberId(FastHash(0x1234_5678)),
            purge_token: crate::room_state::direct_messages::PurgeToken([0xab; 16]),
            timestamp: 1_700_000_000,
            plaintext: "hello, world".to_string(),
        }
    }

    /// Per the "Non-string map keys in JSON-serialized API types" rule
    /// in `freenet/.claude/rules/bug-prevention-patterns.md`, any
    /// wire-boundary type stored in the delegate that may eventually be
    /// JSON-encoded (e.g. by a future diagnostic upload) MUST have a
    /// JSON round-trip test. `OutboundDmStore` uses a `Vec` precisely
    /// for this reason; this test pins that choice.
    #[test]
    fn outbound_dm_store_json_round_trips() {
        let store = OutboundDmStore {
            entries: vec![sample_entry()],
        };
        let json = serde_json::to_string(&store).expect("serialize JSON");
        let parsed: OutboundDmStore = serde_json::from_str(&json).expect("parse JSON");
        assert_eq!(parsed, store);
    }

    /// CBOR is the on-the-wire encoding used by the chat delegate, so
    /// it also has to round-trip.
    #[test]
    fn outbound_dm_store_cbor_round_trips() {
        let store = OutboundDmStore {
            entries: vec![sample_entry(), sample_entry()],
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&store, &mut buf).expect("serialize CBOR");
        let parsed: OutboundDmStore =
            ciborium::de::from_reader(buf.as_slice()).expect("parse CBOR");
        assert_eq!(parsed, store);
    }

    /// An empty store must serialize to a stable, parseable shape so a
    /// fresh delegate can persist a zero-entry store the first time
    /// any caller asks for one.
    #[test]
    fn empty_outbound_dm_store_json_round_trips() {
        let store = OutboundDmStore::default();
        let json = serde_json::to_string(&store).expect("serialize JSON");
        let parsed: OutboundDmStore = serde_json::from_str(&json).expect("parse JSON");
        assert_eq!(parsed, store);
    }
}
