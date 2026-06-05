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
/// thread. Any newer DM (inbound or outbound) crosses the threshold
/// and revives.
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
}
