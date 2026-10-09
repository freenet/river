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
///
/// Piggybacks the `hidden_threads` list (issue freenet/river#261) — a
/// purely local "hide this DM thread from my left rail until a fresh
/// message arrives" view filter. We pack it into the same delegate
/// blob so a single chat-delegate fetch hydrates both, and so a hide
/// on device A is visible on device B without a second storage key.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutboundDmStore {
    #[serde(default)]
    pub entries: Vec<OutboundDmEntry>,
    /// Per-`(room, peer)` "hidden-at" cutoff timestamps. Filter rule:
    /// a thread is hidden iff `hidden_at_ts >= max(message.timestamp)`
    /// for messages between the local user and `peer` in that room.
    /// `#[serde(default)]` so pre-#261 wire bytes (a `Vec<entries>`-only
    /// `OutboundDmStore`) keep decoding into an empty `hidden_threads`.
    #[serde(default)]
    pub hidden_threads: Vec<HiddenDmThreadEntry>,
}

/// A single user-driven "hide this DM thread until further notice" entry.
///
/// `Vec`-of-struct rather than `HashMap` for the same reason as
/// [`OutboundDmStore::entries`] — JSON object keys must serialize as
/// strings (see "Non-string map keys in JSON-serialized API types" in
/// `freenet/.claude/rules/bug-prevention-patterns.md`), and the
/// `(VerifyingKey, MemberId)` lookup tuple does not. The local UI hot
/// path materialises this list into a HashMap for O(1) render-time
/// lookup — see `OutboundDmsCache` in the river-ui crate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HiddenDmThreadEntry {
    /// Room owner verifying key — disambiguates the same peer being a
    /// member of multiple rooms. Raw 32 bytes to match the `RoomKey`
    /// convention used elsewhere in this module and to keep the type
    /// JSON-friendly.
    pub room_owner_vk: [u8; 32],
    /// Counterparty in the DM thread.
    pub peer: MemberId,
    /// Unix seconds at the moment the user clicked "Hide thread".
    /// Captured from the most-recent message timestamp in the thread at
    /// that moment (or `now()` if the thread had no messages yet — an
    /// edge case that can happen if the user composes-and-hides from
    /// the picker without ever sending) so any subsequent message
    /// strictly later than this revives the thread.
    pub hidden_at_ts: u64,
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
///
/// # Byte fields are CBOR byte strings
///
/// Every `Vec<u8>` payload field in this enum and in
/// [`ChatDelegateResponseMsg`] carries `#[serde(with = "cbor_bytes")]`. A bare
/// `Vec<u8>` goes through `serialize_seq`, and ciborium writes it as a CBOR
/// array of integers: every byte >= 0x18 costs 2 bytes, so a 1.18 MB stored
/// room arrived as a 2.32 MB `GetResponse` (freenet/river#757). As a byte
/// string it costs the payload plus a few header bytes.
///
/// Decoding accepts both encodings in both directions, which is what keeps a
/// delegate re-key safe while old and new generations coexist on one node:
/// ciborium's `deserialize_byte_buf` (the [`cbor_bytes`] entry point) also
/// takes a CBOR array, so this UI decodes a legacy delegate's integer-array
/// replies; and ciborium's `deserialize_seq` (the plain `Vec<u8>` entry point)
/// also takes a byte string, so a legacy delegate decodes these requests.
/// Both are pinned in `tests::byte_fields_*`.
///
/// `ChatDelegateKey` is deliberately left as an integer array: keys are sent
/// to legacy delegates during migration (`GetRequest`, `ListRequest`), are a
/// few dozen bytes, and are the one field whose encoding a frozen predecessor
/// has to keep accepting forever.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatDelegateRequestMsg {
    // Key-value storage operations
    StoreRequest {
        key: ChatDelegateKey,
        #[serde(with = "cbor_bytes")]
        value: Vec<u8>,
    },
    GetRequest {
        key: ChatDelegateKey,
    },
    DeleteRequest {
        key: ChatDelegateKey,
    },
    ListRequest,

    // -----------------------------------------------------------------
    // Optimistic-concurrency (compare-and-swap) storage operations.
    //
    // These exist so that multiple concurrent clients (e.g. two browser
    // tabs both editing the room list) cannot silently clobber each
    // other. The plain `StoreRequest` above is a blind last-writer-wins
    // overwrite; a stale tab's full-snapshot write would destroy a
    // newer tab's additions (freenet/river#345). The delegate tracks a
    // per-key generation counter; a `CasStoreRequest` only succeeds when
    // the caller's `expected_generation` matches the stored generation,
    // so a stale writer is rejected and forced to re-read + merge.
    //
    // Appended to the enum (never reordered): ciborium serializes these
    // externally-tagged by variant *name*, and only the current delegate
    // ever receives them, so old delegate WASM is unaffected.
    // -----------------------------------------------------------------
    /// Read a value together with its current generation, so the caller
    /// can subsequently issue a [`CasStoreRequest`] with the matching
    /// `expected_generation`. A missing key reports generation `0`.
    GetVersionedRequest {
        key: ChatDelegateKey,
    },
    /// Store `value` only if the key's current generation equals
    /// `expected_generation` (`0` = expect absent / first write). On a
    /// match the generation is incremented and the value stored; on a
    /// mismatch the store is rejected and the current generation + value
    /// are returned so the caller can merge and retry without an extra
    /// round-trip.
    CasStoreRequest {
        key: ChatDelegateKey,
        #[serde(with = "cbor_bytes")]
        value: Vec<u8>,
        expected_generation: u64,
    },

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
        #[serde(with = "cbor_bytes")]
        message_bytes: Vec<u8>,
    },
    /// Sign a member invitation (Member serialized)
    SignMember {
        room_key: RoomKey,
        request_id: RequestId,
        #[serde(with = "cbor_bytes")]
        member_bytes: Vec<u8>,
    },
    /// Sign a ban (BanV1 serialized)
    SignBan {
        room_key: RoomKey,
        request_id: RequestId,
        #[serde(with = "cbor_bytes")]
        ban_bytes: Vec<u8>,
    },
    /// Sign a room configuration (Configuration serialized)
    SignConfig {
        room_key: RoomKey,
        request_id: RequestId,
        #[serde(with = "cbor_bytes")]
        config_bytes: Vec<u8>,
    },
    /// Sign member info (MemberInfo serialized)
    SignMemberInfo {
        room_key: RoomKey,
        request_id: RequestId,
        #[serde(with = "cbor_bytes")]
        member_info_bytes: Vec<u8>,
    },
    /// Sign a secret version record (SecretVersionRecordV1 serialized)
    SignSecretVersion {
        room_key: RoomKey,
        request_id: RequestId,
        #[serde(with = "cbor_bytes")]
        record_bytes: Vec<u8>,
    },
    /// Sign an encrypted secret for member (EncryptedSecretForMemberV1 serialized)
    SignEncryptedSecret {
        room_key: RoomKey,
        request_id: RequestId,
        #[serde(with = "cbor_bytes")]
        secret_bytes: Vec<u8>,
    },
    /// Sign a room upgrade (RoomUpgrade serialized)
    SignUpgrade {
        room_key: RoomKey,
        request_id: RequestId,
        #[serde(with = "cbor_bytes")]
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
    ///
    /// `request_id` is a per-call unique correlator so the UI's pending-request
    /// registry can route the matching response back to the awaiting future.
    /// Without it, the registry was keyed by `room_owner_vk` only, so a second
    /// `EnsureRoomSubscription` for the same room while a previous one was
    /// still in flight would collide on the same registry slot — the second
    /// caller would receive the first call's response (potentially from a
    /// different session epoch) or have its own response routed to the first
    /// caller. See PR #276 review feedback for the exact race scenario.
    EnsureRoomSubscription {
        room_owner_vk: RoomKey,
        request_id: RequestId,
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
        #[serde(with = "cbor_bytes::option")]
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

    // Compare-and-swap storage responses (see the request variants).
    /// Response to [`ChatDelegateRequestMsg::GetVersionedRequest`].
    GetVersionedResponse {
        key: ChatDelegateKey,
        #[serde(with = "cbor_bytes::option")]
        value: Option<Vec<u8>>,
        /// Current generation of the stored value (`0` if absent).
        generation: u64,
    },
    /// Response to [`ChatDelegateRequestMsg::CasStoreRequest`].
    CasStoreResponse {
        key: ChatDelegateKey,
        result: CasStoreResult,
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
    ///
    /// `request_id` is echoed back from the request so the UI can route the
    /// response to the specific awaiting future (see the doc-comment on the
    /// request variant for why a per-request correlator is required).
    EnsureRoomSubscriptionResponse {
        room_owner_vk: RoomKey,
        request_id: RequestId,
        result: Result<(), String>,
    },
}

/// Outcome of a [`ChatDelegateRequestMsg::CasStoreRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CasStoreResult {
    /// The compare-and-swap succeeded; carries the new generation after
    /// the write (the caller should remember it for its next store).
    Stored { generation: u64 },
    /// Generation mismatch — the store was rejected because another
    /// writer advanced the key first. Carries the current generation and
    /// value so the caller can merge its pending changes and retry with
    /// `expected_generation = current_generation`.
    Conflict {
        current_generation: u64,
        #[serde(with = "cbor_bytes::option")]
        current_value: Option<Vec<u8>>,
    },
    /// The host-function store failed (e.g. secret storage error).
    Failed(String),
}

/// Pure helper: should a DM thread for `(room, peer)` currently be
/// hidden from the left rail?
///
/// Returns `true` iff the user has a `HiddenDmThreadEntry` for the
/// thread AND no message in the thread has `timestamp > hidden_at_ts`.
/// The strict `>` (not `>=`) on `max_message_ts` ensures that the
/// message used to populate `hidden_at_ts` does not itself revive the
/// thread. Any newer INBOUND DM crosses the threshold and revives;
/// outbound sends revive via the explicit `unhide_dm_thread` instead,
/// since freenet/river#526 made the archive clock inbound-only.
///
/// `hidden_threads` is the full slice as loaded from the delegate;
/// the lookup is linear because the list is tiny (bounded by the
/// number of distinct DM pairs the user has actually hidden, which
/// in practice is well under a hundred). Issue freenet/river#261.
pub fn is_thread_hidden(
    hidden_threads: &[HiddenDmThreadEntry],
    room_owner_vk: &[u8; 32],
    peer: MemberId,
    max_message_ts: u64,
) -> bool {
    hidden_threads
        .iter()
        .find(|h| &h.room_owner_vk == room_owner_vk && h.peer == peer)
        .is_some_and(|h| max_message_ts <= h.hidden_at_ts)
}

/// Serde helper for the byte fields of [`ChatDelegateRequestMsg`] /
/// [`ChatDelegateResponseMsg`]: encode as a CBOR **byte string**, decode
/// either a byte string or the legacy array-of-integers form.
///
/// Hand-rolled rather than the `serde_bytes` crate on purpose: adding that
/// dependency to river-core re-keyed the ROOM CONTRACT even though the room
/// contract never touches these types (measured for #757: `room_contract.wasm`
/// went from `a3e63c8c…` to `48d91e7b…`, same size, functions reordered —
/// most likely via river-core's crate metadata hash feeding symbol order).
/// This module leaves the room contract byte-identical.
/// The `check-room-contract-migration` CI gate catches that, but a
/// delegate-only change has no reason to pay for a room-contract re-key.
///
/// `deserialize_byte_buf` is ciborium's entry point that accepts both a CBOR
/// byte string and a CBOR array, so this is the decode-both path without
/// `deserialize_any`. These messages are only ever encoded with ciborium.
mod cbor_bytes {
    use serde::de::{SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        deserializer.deserialize_byte_buf(BytesOrLegacySeq)
    }

    struct BytesOrLegacySeq;

    impl<'de> Visitor<'de> for BytesOrLegacySeq {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a CBOR byte string, or a legacy array of byte values")
        }

        fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
            Ok(v.to_vec())
        }

        fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
            Ok(v)
        }

        /// Legacy form: a CBOR array of integers, one per byte.
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            // `size_hint` is the array header's DECLARED length, unchecked
            // against the input, so cap the preallocation (the same bound
            // serde's own `Vec<u8>` impl applies). See `payload_bytes` in
            // `room_state/content.rs` for the lying-header case.
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
            while let Some(byte) = seq.next_element::<u8>()? {
                out.push(byte);
            }
            Ok(out)
        }
    }

    /// The same encoding for `Option<Vec<u8>>`: `None` stays CBOR `null`.
    pub mod option {
        use serde::{Deserialize, Deserializer, Serialize, Serializer};

        struct BytesRef<'a>(&'a [u8]);

        impl Serialize for BytesRef<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_bytes(self.0)
            }
        }

        #[derive(Deserialize)]
        struct Bytes(#[serde(with = "super")] Vec<u8>);

        pub fn serialize<S: Serializer>(
            value: &Option<Vec<u8>>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(bytes) => serializer.serialize_some(&BytesRef(bytes)),
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Vec<u8>>, D::Error> {
            Ok(Option::<Bytes>::deserialize(deserializer)?.map(|b| b.0))
        }
    }
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

    fn sample_hidden() -> HiddenDmThreadEntry {
        HiddenDmThreadEntry {
            room_owner_vk: [9u8; 32],
            peer: MemberId(FastHash(0x1234_5678)),
            hidden_at_ts: 1_700_000_000,
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
            hidden_threads: vec![],
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
            hidden_threads: vec![],
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

    /// Issue freenet/river#261 — `hidden_threads` is now part of the
    /// stored blob. JSON round-trip pins the load-bearing wire shape
    /// (Vec of struct, not HashMap) per the "non-string map keys"
    /// bug-prevention pattern.
    #[test]
    fn outbound_dm_store_with_hidden_threads_json_round_trips() {
        let store = OutboundDmStore {
            entries: vec![sample_entry()],
            hidden_threads: vec![sample_hidden()],
        };
        let json = serde_json::to_string(&store).expect("serialize JSON");
        let parsed: OutboundDmStore = serde_json::from_str(&json).expect("parse JSON");
        assert_eq!(parsed, store);
    }

    /// CBOR is the on-the-wire encoding used by the chat delegate, so
    /// `hidden_threads` must also CBOR round-trip.
    #[test]
    fn outbound_dm_store_with_hidden_threads_cbor_round_trips() {
        let store = OutboundDmStore {
            entries: vec![],
            hidden_threads: vec![sample_hidden(), sample_hidden()],
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&store, &mut buf).expect("serialize CBOR");
        let parsed: OutboundDmStore =
            ciborium::de::from_reader(buf.as_slice()).expect("parse CBOR");
        assert_eq!(parsed, store);
    }

    /// Issue freenet/river#261 BACKWARDS COMPAT: pre-#261 delegate
    /// blobs serialized BEFORE `hidden_threads` existed must still
    /// decode into an `OutboundDmStore` with an empty `hidden_threads`
    /// (via `#[serde(default)]`). Without this, the first reload
    /// after upgrading River would fail to hydrate the outbound-DM
    /// cache for every user whose delegate already has the #256 blob.
    ///
    /// We pin both JSON and CBOR: JSON via a hand-written legacy
    /// payload (the shape `serde_json::to_string` would have produced
    /// before this PR), and CBOR by serializing a synthetic
    /// "legacy" store that contains only the `entries` field via the
    /// same path the delegate writes.
    #[test]
    fn outbound_dm_store_decodes_legacy_json_without_hidden_threads() {
        let legacy_json = r#"{"entries":[]}"#;
        let parsed: OutboundDmStore =
            serde_json::from_str(legacy_json).expect("legacy JSON must decode");
        assert!(parsed.entries.is_empty());
        assert!(parsed.hidden_threads.is_empty());
    }

    #[test]
    fn outbound_dm_store_decodes_legacy_cbor_without_hidden_threads() {
        // Simulate a pre-#261 OutboundDmStore wire shape by hand-rolling
        // a CBOR map with only the `entries` key. `ciborium` writes
        // structs as definite-length maps keyed by field name, so we
        // reproduce that here:
        //   { "entries": [ <one OutboundDmEntry> ] }
        #[derive(Serialize)]
        struct LegacyStore {
            entries: Vec<OutboundDmEntry>,
        }
        let legacy = LegacyStore {
            entries: vec![sample_entry()],
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&legacy, &mut buf).expect("serialize legacy CBOR");

        let parsed: OutboundDmStore =
            ciborium::de::from_reader(buf.as_slice()).expect("legacy CBOR must decode");
        assert_eq!(parsed.entries.len(), 1);
        assert!(parsed.hidden_threads.is_empty());
    }

    /// `is_thread_hidden` returns false on an empty hidden list. This
    /// is the common-case fast-path for users who have never hidden a
    /// thread.
    #[test]
    fn is_thread_hidden_returns_false_for_empty_list() {
        let peer = MemberId(FastHash(0x42));
        assert!(!is_thread_hidden(&[], &[0u8; 32], peer, 0));
        assert!(!is_thread_hidden(&[], &[0u8; 32], peer, 1_000));
    }

    /// `is_thread_hidden` returns true when the only message in the
    /// thread is the one whose timestamp was captured as
    /// `hidden_at_ts`. The strict `>` rule means equal-timestamp does
    /// NOT revive — otherwise hiding a thread whose most-recent message
    /// is exactly `now()` would instantly fail to hide.
    #[test]
    fn is_thread_hidden_equal_timestamp_stays_hidden() {
        let peer = MemberId(FastHash(0x42));
        let hidden = vec![HiddenDmThreadEntry {
            room_owner_vk: [9u8; 32],
            peer,
            hidden_at_ts: 1_000,
        }];
        assert!(is_thread_hidden(&hidden, &[9u8; 32], peer, 1_000));
    }

    /// Any message strictly later than `hidden_at_ts` must revive the
    /// thread.
    #[test]
    fn is_thread_hidden_strictly_later_message_revives() {
        let peer = MemberId(FastHash(0x42));
        let hidden = vec![HiddenDmThreadEntry {
            room_owner_vk: [9u8; 32],
            peer,
            hidden_at_ts: 1_000,
        }];
        assert!(!is_thread_hidden(&hidden, &[9u8; 32], peer, 1_001));
    }

    /// A `HiddenDmThreadEntry` for the same peer in a DIFFERENT room
    /// must NOT hide the thread in the current room. The lookup is
    /// `(room, peer)`, not just `peer`.
    #[test]
    fn is_thread_hidden_is_scoped_per_room() {
        let peer = MemberId(FastHash(0x42));
        let hidden = vec![HiddenDmThreadEntry {
            room_owner_vk: [9u8; 32],
            peer,
            hidden_at_ts: 1_000,
        }];
        // Different room — must be visible.
        assert!(!is_thread_hidden(&hidden, &[7u8; 32], peer, 500));
    }

    /// A `HiddenDmThreadEntry` for a DIFFERENT peer in the same room
    /// must NOT hide the thread.
    #[test]
    fn is_thread_hidden_is_scoped_per_peer() {
        let peer_a = MemberId(FastHash(0x42));
        let peer_b = MemberId(FastHash(0x99));
        let hidden = vec![HiddenDmThreadEntry {
            room_owner_vk: [9u8; 32],
            peer: peer_a,
            hidden_at_ts: 1_000,
        }];
        assert!(!is_thread_hidden(&hidden, &[9u8; 32], peer_b, 500));
    }

    /// Thread with no messages at all (max_message_ts = 0) and a
    /// `hidden_at_ts` of 0 stays hidden — the strict `<=` rule still
    /// applies. This matches the design intent: a freshly hidden
    /// empty thread should stay hidden until either party sends a
    /// (necessarily later, since unix ts > 0) message.
    #[test]
    fn is_thread_hidden_zero_max_zero_hidden_stays_hidden() {
        let peer = MemberId(FastHash(0x42));
        let hidden = vec![HiddenDmThreadEntry {
            room_owner_vk: [9u8; 32],
            peer,
            hidden_at_ts: 0,
        }];
        assert!(is_thread_hidden(&hidden, &[9u8; 32], peer, 0));
    }

    // ------------------------------------------------------------------
    // CAS wire-format round-trips (freenet/river#345).
    //
    // The chat-delegate request/response enums had no dedicated wire test
    // before this — they were only exercised end-to-end. These pin the
    // new compare-and-swap variants so a future serde/ciborium change
    // can't silently break the protocol between the UI and the delegate.
    // ------------------------------------------------------------------

    fn cbor_round_trip_request(msg: &ChatDelegateRequestMsg) -> ChatDelegateRequestMsg {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(msg, &mut buf).expect("serialize request");
        ciborium::from_reader(buf.as_slice()).expect("deserialize request")
    }

    fn cbor_round_trip_response(msg: &ChatDelegateResponseMsg) -> ChatDelegateResponseMsg {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(msg, &mut buf).expect("serialize response");
        ciborium::from_reader(buf.as_slice()).expect("deserialize response")
    }

    #[test]
    fn cas_store_request_cbor_round_trips() {
        let msg = ChatDelegateRequestMsg::CasStoreRequest {
            key: ChatDelegateKey(b"rooms_data".to_vec()),
            value: vec![1, 2, 3, 4, 5],
            expected_generation: 7,
        };
        match cbor_round_trip_request(&msg) {
            ChatDelegateRequestMsg::CasStoreRequest {
                key,
                value,
                expected_generation,
            } => {
                assert_eq!(key.as_bytes(), b"rooms_data");
                assert_eq!(value, vec![1, 2, 3, 4, 5]);
                assert_eq!(expected_generation, 7);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn get_versioned_request_cbor_round_trips() {
        let msg = ChatDelegateRequestMsg::GetVersionedRequest {
            key: ChatDelegateKey(b"rooms_data".to_vec()),
        };
        assert!(matches!(
            cbor_round_trip_request(&msg),
            ChatDelegateRequestMsg::GetVersionedRequest { .. }
        ));
    }

    #[test]
    fn cas_store_result_stored_round_trips() {
        let msg = ChatDelegateResponseMsg::CasStoreResponse {
            key: ChatDelegateKey(b"rooms_data".to_vec()),
            result: CasStoreResult::Stored { generation: 42 },
        };
        match cbor_round_trip_response(&msg) {
            ChatDelegateResponseMsg::CasStoreResponse {
                result: CasStoreResult::Stored { generation },
                ..
            } => assert_eq!(generation, 42),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn cas_store_result_conflict_round_trips() {
        let msg = ChatDelegateResponseMsg::CasStoreResponse {
            key: ChatDelegateKey(b"rooms_data".to_vec()),
            result: CasStoreResult::Conflict {
                current_generation: 9,
                current_value: Some(vec![0xaa, 0xbb]),
            },
        };
        match cbor_round_trip_response(&msg) {
            ChatDelegateResponseMsg::CasStoreResponse {
                result:
                    CasStoreResult::Conflict {
                        current_generation,
                        current_value,
                    },
                ..
            } => {
                assert_eq!(current_generation, 9);
                assert_eq!(current_value, Some(vec![0xaa, 0xbb]));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn get_versioned_response_round_trips() {
        let msg = ChatDelegateResponseMsg::GetVersionedResponse {
            key: ChatDelegateKey(b"rooms_data".to_vec()),
            value: Some(vec![1, 2, 3]),
            generation: 5,
        };
        match cbor_round_trip_response(&msg) {
            ChatDelegateResponseMsg::GetVersionedResponse {
                value, generation, ..
            } => {
                assert_eq!(value, Some(vec![1, 2, 3]));
                assert_eq!(generation, 5);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// Appending the CAS variants must not disturb the existing variants:
    /// a plain `StoreRequest`/`GetResponse` still round-trips unchanged.
    /// (ciborium tags externally by name, so this holds by construction —
    /// the test pins it against an accidental `#[serde(...)]` change.)
    #[test]
    fn legacy_variants_still_round_trip_after_appending_cas() {
        let store = ChatDelegateRequestMsg::StoreRequest {
            key: ChatDelegateKey(b"outbound_dms".to_vec()),
            value: vec![9, 9, 9],
        };
        assert!(matches!(
            cbor_round_trip_request(&store),
            ChatDelegateRequestMsg::StoreRequest { .. }
        ));
        let get = ChatDelegateResponseMsg::GetResponse {
            key: ChatDelegateKey(b"outbound_dms".to_vec()),
            value: Some(vec![9, 9, 9]),
        };
        assert!(matches!(
            cbor_round_trip_response(&get),
            ChatDelegateResponseMsg::GetResponse { .. }
        ));
    }

    // ---------------------------------------------------------------------
    // Byte-field encoding (freenet/river#757)
    //
    // `legacy` mirrors the pre-#757 shape of every variant whose byte fields
    // gained `serde(with = "cbor_bytes")`: the same variant names and field
    // names with a plain `Vec<u8>`, which is exactly what every predecessor
    // delegate generation (and the UI that talked to it) was compiled with.
    // ciborium tags enums externally by variant NAME, so a subset enum is
    // wire-identical for the variants it names.
    // ---------------------------------------------------------------------
    // Variant names must match the live enums exactly (ciborium tags by name).
    #[allow(clippy::enum_variant_names)]
    mod legacy {
        use super::super::{RequestId, RoomKey};
        use serde::{Deserialize, Serialize};

        /// Its own key type, NOT the live `ChatDelegateKey`: sharing the live
        /// type would make any encoding change to it invisible here.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub struct ChatDelegateKey(pub Vec<u8>);

        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub enum Request {
            StoreRequest {
                key: ChatDelegateKey,
                value: Vec<u8>,
            },
            CasStoreRequest {
                key: ChatDelegateKey,
                value: Vec<u8>,
                expected_generation: u64,
            },
            SignMessage {
                room_key: RoomKey,
                request_id: RequestId,
                message_bytes: Vec<u8>,
            },
            SignMember {
                room_key: RoomKey,
                request_id: RequestId,
                member_bytes: Vec<u8>,
            },
            SignBan {
                room_key: RoomKey,
                request_id: RequestId,
                ban_bytes: Vec<u8>,
            },
            SignConfig {
                room_key: RoomKey,
                request_id: RequestId,
                config_bytes: Vec<u8>,
            },
            SignMemberInfo {
                room_key: RoomKey,
                request_id: RequestId,
                member_info_bytes: Vec<u8>,
            },
            SignSecretVersion {
                room_key: RoomKey,
                request_id: RequestId,
                record_bytes: Vec<u8>,
            },
            SignEncryptedSecret {
                room_key: RoomKey,
                request_id: RequestId,
                secret_bytes: Vec<u8>,
            },
            SignUpgrade {
                room_key: RoomKey,
                request_id: RequestId,
                upgrade_bytes: Vec<u8>,
            },
            GetRequest {
                key: ChatDelegateKey,
            },
            ListRequest,
        }

        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub enum Response {
            GetResponse {
                key: ChatDelegateKey,
                value: Option<Vec<u8>>,
            },
            GetVersionedResponse {
                key: ChatDelegateKey,
                value: Option<Vec<u8>>,
                generation: u64,
            },
            CasStoreResponse {
                key: ChatDelegateKey,
                result: CasStoreResult,
            },
            ListResponse {
                keys: Vec<ChatDelegateKey>,
            },
        }

        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        pub enum CasStoreResult {
            Stored {
                generation: u64,
            },
            Conflict {
                current_generation: u64,
                current_value: Option<Vec<u8>>,
            },
            Failed(String),
        }
    }

    fn lk(key: &ChatDelegateKey) -> legacy::ChatDelegateKey {
        legacy::ChatDelegateKey(key.0.clone())
    }

    fn to_cbor<T: Serialize>(v: &T) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(v, &mut buf).expect("serialize CBOR");
        buf
    }

    fn from_cbor<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> T {
        ciborium::de::from_reader(bytes).expect("parse CBOR")
    }

    /// Every byte value, so the high (2-byte-as-integer) range is covered.
    fn all_bytes() -> Vec<u8> {
        (0..=255u8).cycle().take(1000).collect()
    }

    /// Re-encode a new-format value through its legacy mirror and back, so a
    /// test can compare the two without needing `PartialEq` on the new enums.
    fn new_request_as_legacy(msg: &ChatDelegateRequestMsg) -> legacy::Request {
        from_cbor(&to_cbor(msg))
    }

    fn new_response_as_legacy(msg: &ChatDelegateResponseMsg) -> legacy::Response {
        from_cbor(&to_cbor(msg))
    }

    /// The point of #757: a large value costs ~its own size on the wire, not
    /// ~2x. 1000 bytes covering every byte value is 1002 bytes of CBOR string
    /// plus the envelope; as an integer array it was ~1.9 KB.
    #[test]
    fn byte_fields_encode_as_cbor_byte_strings() {
        let value = all_bytes();
        let new = to_cbor(&ChatDelegateResponseMsg::GetResponse {
            key: ChatDelegateKey(b"k".to_vec()),
            value: Some(value.clone()),
        });
        let old = to_cbor(&legacy::Response::GetResponse {
            key: legacy::ChatDelegateKey(b"k".to_vec()),
            value: Some(value.clone()),
        });
        // 0x59 0x03 0xE8 = CBOR major type 2 (byte string), 2-byte length 1000.
        assert!(
            new.windows(3).any(|w| w == [0x59, 0x03, 0xE8]),
            "value must be a CBOR byte string"
        );
        assert!(
            new.len() < value.len() + 64,
            "new encoding is {} bytes",
            new.len()
        );
        assert!(
            old.len() > value.len() * 18 / 10,
            "legacy encoding is {} bytes",
            old.len()
        );
    }

    /// A predecessor delegate replies with integer arrays; the new UI must
    /// decode them, or every migration read of a legacy delegate fails.
    /// Covers every response field that changed, including `None`.
    #[test]
    fn byte_fields_decode_legacy_integer_array_responses() {
        let value = all_bytes();
        let key = ChatDelegateKey(b"room:abc".to_vec());
        for v in [Some(value.clone()), Some(vec![]), None] {
            let legacy_get = legacy::Response::GetResponse {
                key: lk(&key),
                value: v.clone(),
            };
            match from_cbor::<ChatDelegateResponseMsg>(&to_cbor(&legacy_get)) {
                ChatDelegateResponseMsg::GetResponse { key: k, value: got } => {
                    assert_eq!(k, key);
                    assert_eq!(got, v);
                }
                other => panic!("wrong variant: {other:?}"),
            }

            let legacy_versioned = legacy::Response::GetVersionedResponse {
                key: lk(&key),
                value: v.clone(),
                generation: 7,
            };
            match from_cbor::<ChatDelegateResponseMsg>(&to_cbor(&legacy_versioned)) {
                ChatDelegateResponseMsg::GetVersionedResponse {
                    value: got,
                    generation,
                    ..
                } => {
                    assert_eq!(got, v);
                    assert_eq!(generation, 7);
                }
                other => panic!("wrong variant: {other:?}"),
            }

            let legacy_conflict = legacy::Response::CasStoreResponse {
                key: lk(&key),
                result: legacy::CasStoreResult::Conflict {
                    current_generation: 3,
                    current_value: v.clone(),
                },
            };
            match from_cbor::<ChatDelegateResponseMsg>(&to_cbor(&legacy_conflict)) {
                ChatDelegateResponseMsg::CasStoreResponse { result, .. } => assert_eq!(
                    result,
                    CasStoreResult::Conflict {
                        current_generation: 3,
                        current_value: v.clone(),
                    }
                ),
                other => panic!("wrong variant: {other:?}"),
            }
        }
    }

    /// The reverse direction: a frozen predecessor delegate (plain `Vec<u8>`)
    /// must still decode what the new UI sends and what the new delegate
    /// replies. The migration path sends predecessors only key-only requests
    /// today, so this is defence in depth, not a load-bearing path — but it
    /// means nothing in the protocol depends on that staying true.
    #[test]
    fn byte_fields_legacy_decoder_accepts_byte_strings() {
        let value = all_bytes();
        let key = ChatDelegateKey(b"room:abc".to_vec());
        let room_key = [5u8; 32];

        let cases: Vec<(ChatDelegateRequestMsg, legacy::Request)> = vec![
            (
                ChatDelegateRequestMsg::StoreRequest {
                    key: key.clone(),
                    value: value.clone(),
                },
                legacy::Request::StoreRequest {
                    key: lk(&key),
                    value: value.clone(),
                },
            ),
            (
                ChatDelegateRequestMsg::CasStoreRequest {
                    key: key.clone(),
                    value: value.clone(),
                    expected_generation: 4,
                },
                legacy::Request::CasStoreRequest {
                    key: lk(&key),
                    value: value.clone(),
                    expected_generation: 4,
                },
            ),
            (
                ChatDelegateRequestMsg::SignMessage {
                    room_key,
                    request_id: 1,
                    message_bytes: value.clone(),
                },
                legacy::Request::SignMessage {
                    room_key,
                    request_id: 1,
                    message_bytes: value.clone(),
                },
            ),
            (
                ChatDelegateRequestMsg::SignMember {
                    room_key,
                    request_id: 2,
                    member_bytes: value.clone(),
                },
                legacy::Request::SignMember {
                    room_key,
                    request_id: 2,
                    member_bytes: value.clone(),
                },
            ),
            (
                ChatDelegateRequestMsg::SignBan {
                    room_key,
                    request_id: 3,
                    ban_bytes: value.clone(),
                },
                legacy::Request::SignBan {
                    room_key,
                    request_id: 3,
                    ban_bytes: value.clone(),
                },
            ),
            (
                ChatDelegateRequestMsg::SignConfig {
                    room_key,
                    request_id: 4,
                    config_bytes: value.clone(),
                },
                legacy::Request::SignConfig {
                    room_key,
                    request_id: 4,
                    config_bytes: value.clone(),
                },
            ),
            (
                ChatDelegateRequestMsg::SignMemberInfo {
                    room_key,
                    request_id: 5,
                    member_info_bytes: value.clone(),
                },
                legacy::Request::SignMemberInfo {
                    room_key,
                    request_id: 5,
                    member_info_bytes: value.clone(),
                },
            ),
            (
                ChatDelegateRequestMsg::SignSecretVersion {
                    room_key,
                    request_id: 6,
                    record_bytes: value.clone(),
                },
                legacy::Request::SignSecretVersion {
                    room_key,
                    request_id: 6,
                    record_bytes: value.clone(),
                },
            ),
            (
                ChatDelegateRequestMsg::SignEncryptedSecret {
                    room_key,
                    request_id: 7,
                    secret_bytes: value.clone(),
                },
                legacy::Request::SignEncryptedSecret {
                    room_key,
                    request_id: 7,
                    secret_bytes: value.clone(),
                },
            ),
            (
                ChatDelegateRequestMsg::SignUpgrade {
                    room_key,
                    request_id: 8,
                    upgrade_bytes: value.clone(),
                },
                legacy::Request::SignUpgrade {
                    room_key,
                    request_id: 8,
                    upgrade_bytes: value.clone(),
                },
            ),
        ];
        for (new, expected) in cases {
            assert_eq!(new_request_as_legacy(&new), expected);
            // And the legacy form still decodes into the new type with the
            // same bytes (re-encoded through legacy for comparison).
            let back: ChatDelegateRequestMsg = from_cbor(&to_cbor(&expected));
            assert_eq!(new_request_as_legacy(&back), expected);
        }

        for v in [Some(value.clone()), Some(vec![]), None] {
            assert_eq!(
                new_response_as_legacy(&ChatDelegateResponseMsg::GetResponse {
                    key: key.clone(),
                    value: v.clone(),
                }),
                legacy::Response::GetResponse {
                    key: lk(&key),
                    value: v.clone()
                }
            );
            assert_eq!(
                new_response_as_legacy(&ChatDelegateResponseMsg::GetVersionedResponse {
                    key: key.clone(),
                    value: v.clone(),
                    generation: 9,
                }),
                legacy::Response::GetVersionedResponse {
                    key: lk(&key),
                    value: v.clone(),
                    generation: 9,
                }
            );
            assert_eq!(
                new_response_as_legacy(&ChatDelegateResponseMsg::CasStoreResponse {
                    key: key.clone(),
                    result: CasStoreResult::Conflict {
                        current_generation: 2,
                        current_value: v.clone(),
                    },
                }),
                legacy::Response::CasStoreResponse {
                    key: lk(&key),
                    result: legacy::CasStoreResult::Conflict {
                        current_generation: 2,
                        current_value: v.clone(),
                    },
                }
            );
        }
    }

    /// The requests the migration walk sends to a PREDECESSOR delegate must be
    /// byte-identical to what that predecessor's own UI sent, because a frozen
    /// WASM is the one decoder we can never update. Today that is the key-only
    /// `GetRequest` and `ListRequest`; `ChatDelegateKey` stays an integer
    /// array for exactly this reason.
    #[test]
    fn predecessor_bound_requests_are_wire_identical_to_legacy() {
        let key = ChatDelegateKey(b"room:3xYz".to_vec());
        assert_eq!(
            to_cbor(&ChatDelegateRequestMsg::GetRequest { key: key.clone() }),
            to_cbor(&legacy::Request::GetRequest { key: lk(&key) })
        );
        assert_eq!(
            to_cbor(&ChatDelegateRequestMsg::ListRequest),
            to_cbor(&legacy::Request::ListRequest)
        );
        assert_eq!(
            to_cbor(&ChatDelegateResponseMsg::ListResponse {
                keys: vec![key.clone()]
            }),
            to_cbor(&legacy::Response::ListResponse {
                keys: vec![lk(&key)]
            })
        );
    }

    /// A legacy array header declares its length, and ciborium hands that
    /// declared length to `size_hint` unchecked. A tiny message claiming
    /// 2^64-1 elements must fail to decode, not allocate or panic.
    #[test]
    fn legacy_array_with_lying_length_header_errors_not_panics() {
        // {"GetResponse": {"key": [], "value": <array, 2^64-1 elements>}}
        let mut bytes = to_cbor(&legacy::Response::GetResponse {
            key: legacy::ChatDelegateKey(vec![]),
            value: Some(vec![]),
        });
        // The empty legacy `value` array is the final byte (0x80); replace it
        // with an 8-byte-length array header and a single element.
        assert_eq!(bytes.pop(), Some(0x80));
        bytes.extend_from_slice(&[0x9B, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]);
        let decoded: Result<ChatDelegateResponseMsg, _> =
            ciborium::de::from_reader(bytes.as_slice());
        assert!(decoded.is_err());
    }
}
