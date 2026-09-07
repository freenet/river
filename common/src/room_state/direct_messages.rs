//! In-room direct messages (#230 Phase 1).
//!
//! End-to-end-encrypted DMs between two members of the same room,
//! carried inside `ChatRoomStateV1`. Replaces the reverted inbox-contract
//! approach (PR #234 → reverted in #238) - instead of a separate per-pair
//! contract, DMs live in the room contract and are scoped to the room
//! they're sent in by design.
//!
//! # State shape
//!
//! - [`DirectMessagesV1::messages`]: a flat list of
//!   [`AuthorizedDirectMessage`]s. Each is signed by its sender,
//!   addressed to a specific recipient, and carries opaque ECIES
//!   ciphertext encrypted to the recipient's `member_vk`.
//!
//! - [`DirectMessagesV1::purges`]: a sorted list of
//!   [`AuthorizedRecipientPurges`] tombstone envelopes, one per
//!   recipient. Each recipient signs a single, monotonically-versioned
//!   list of [`PurgeToken`] entries identifying messages they've purged.
//!   The recipient is the sole signer of their own purge envelope;
//!   concurrent updates are resolved by strict-monotonic `version`. A
//!   `Vec` (rather than `HashMap<MemberId, _>`) is used so the state
//!   round-trips through `serde_json` - `MemberId` is a struct and is
//!   rejected as a JSON object key (see bug-prevention-patterns
//!   "Non-string map keys", #3987 incident).
//!
//! # Authorisation model
//!
//! Every piece of state is cryptographically authorised at insertion:
//!
//! 1. Each [`AuthorizedDirectMessage`] carries a sender signature over
//!    canonical bytes (see [`build_direct_message_signed_bytes`]) that
//!    bind `sender`, `recipient`, `room_owner_vk`, `timestamp`, and
//!    `ciphertext`, prefixed by the 1-byte domain tag
//!    [`DOMAIN_TAG_MESSAGE`]. The signature is verified against the
//!    sender's resolved `member_vk` (looked up in
//!    `parent_state.members`).
//!
//! 2. Each [`AuthorizedRecipientPurges`] carries a recipient signature
//!    over canonical bytes (see [`build_recipient_purges_signed_bytes`])
//!    that bind `recipient`, `room_owner_vk`, `version`, and the purge
//!    list, prefixed by the 1-byte domain tag [`DOMAIN_TAG_PURGES`].
//!    Verified against the recipient's resolved `member_vk`.
//!
//! 3. Both sender and recipient MUST be current members of the room.
//!    The owner is treated as an implicit member (their key is in
//!    `parameters.owner`). Bans are NOT enforced here - see "Interaction
//!    with bans" below.
//!
//! # Tombstone-as-block semantics
//!
//! Once a recipient signs a purge envelope listing the BLAKE3-derived
//! [`PurgeToken`] of a sender's signature, ANY incoming message whose
//! signature hashes to the same token is dropped on merge. Versioning of
//! the purge envelope follows the `Configuration` monotonic-version
//! pattern (one signed envelope per recipient, strictly-greater version
//! replaces older); the drop-on-merge filtering effect matches `BansV1`'s
//! treatment of banned members. Stale peers re-merging a purged message
//! are blocked by the current `purges` state. Each new envelope MUST
//! contain a superset of the previous version's tombstones (no
//! un-purging) - enforced in [`ComposableState::apply_delta`].
//!
//! # Interaction with bans
//!
//! `verify` deliberately does NOT reject DMs whose sender or recipient
//! is currently in `parent_state.bans` - same precedent as
//! [`crate::room_state::message::MessagesV1`], which only checks
//! signatures + author-is-a-member in `verify`. Bans are enforced as a
//! *sweep* in [`crate::ChatRoomStateV1::post_apply_cleanup`]: banned DMs
//! are dropped after each merge so the state stays verifiable. Without
//! this split, adding a ban for a participant of an existing DM would
//! make every peer's verify fail until the next purge - a self-DoS.
//!
//! # Threat model
//!
//! - The contract validates only the OUTER envelope (sender authorised,
//!   recipient is a member of the same room, caps respected, tombstones
//!   honoured). The inner ECIES ciphertext is OPAQUE - the contract
//!   cannot read it, has no view into per-message replay, and provides
//!   no in-contract de-duplication of identical re-sent ciphertexts.
//!
//! - A malicious member can grief storage by saturating their own
//!   per-pair cap (up to [`MAX_DM_MESSAGES_PER_PAIR`] ×
//!   [`MAX_DM_CIPHERTEXT_BYTES`] per recipient they target). The
//!   recipient mitigates by signing a purge envelope listing the
//!   offending tokens.
//!
//! - Re-spam after purge is NOT prevented - a banned-then-unbanned (or
//!   simply persistent) member produces a fresh signature on each DM,
//!   yielding a fresh purge token. Tombstones prevent state-replay
//!   ("stale peer re-merges the same signed message") but not new spam;
//!   that's a ban concern.
//!
//! # Bounds
//!
//! - `Configuration::effective_max_direct_messages`: owner-tunable GLOBAL cap
//!   on `messages`, defaulting to
//!   [`crate::room_state::configuration::DEFAULT_MAX_DIRECT_MESSAGES`] (300).
//!   An INTERIM bound — the intended fix is moving DMs into per-member
//!   contracts — so it is deliberately the simplest correct thing and is kept
//!   wholly inside this module. Added
//!   for freenet/river#519: the per-pair cap below bounds any one
//!   conversation but nothing bounded the set as a whole, and because
//!   `ChatRoomStateV1::post_apply_cleanup` exempts every DM participant from
//!   inactivity-prune (via [`DirectMessagesV1::participants_of_surviving_dms`]), an
//!   unbounded DM set pinned an unbounded member set. Enforced in
//!   `apply_delta` only, NEVER in `verify` — see the note on
//!   [`DmRetentionHorizon`] and `trim_to_global_cap`.
//! - [`MAX_DM_MESSAGES_PER_PAIR`]: per (sender, recipient) ordered pair.
//! - [`MAX_DM_CIPHERTEXT_BYTES`]: per-message ciphertext size cap.
//! - [`MAX_PURGED_TOMBSTONES_PER_RECIPIENT`]: cap on per-recipient
//!   purge-list length.
//! - [`MAX_DM_FUTURE_SKEW_SECS`]: maximum permitted future-skew when
//!   accepting a fresh message (verifiable via
//!   [`check_dm_future_skew`]). Not enforced inside `verify` (would be
//!   self-DoS for already-stored state).

use crate::room_state::ban::{AuthorizedUserBan, BansV1};
use crate::room_state::member::{AuthorizedMember, MemberId};
use crate::room_state::ChatRoomParametersV1;
use crate::ChatRoomStateV1;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

// ---------------------------------------------------------------------------
// Domain separation tags (prepended to signed byte buffers)
// ---------------------------------------------------------------------------

/// Domain-separation tag for [`build_direct_message_signed_bytes`]. The
/// signed buffer always begins with this byte so a sender's DM signature
/// can never be reused as a recipient purge signature (or vice versa)
/// regardless of crafted field lengths.
pub const DOMAIN_TAG_MESSAGE: u8 = b'M';

/// Domain-separation tag for [`build_recipient_purges_signed_bytes`].
pub const DOMAIN_TAG_PURGES: u8 = b'P';

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Maximum direct messages held per ordered `(sender, recipient)` pair.
pub const MAX_DM_MESSAGES_PER_PAIR: usize = 100;

/// Maximum permitted ciphertext size per direct message, in bytes.
pub const MAX_DM_CIPHERTEXT_BYTES: usize = 32_768;

/// Maximum tombstone entries any single recipient may keep.
pub const MAX_PURGED_TOMBSTONES_PER_RECIPIENT: usize = 1000;

/// Maximum permitted future-skew when ingesting a fresh direct message
/// (seconds). Use [`check_dm_future_skew`] at message-construction time;
/// `verify` deliberately does NOT enforce this on already-stored state
/// to avoid self-DoS.
pub const MAX_DM_FUTURE_SKEW_SECS: u64 = 5 * 60;

// ---------------------------------------------------------------------------
// PurgeToken - BLAKE3-derived signature tombstone
// ---------------------------------------------------------------------------

/// 16-byte BLAKE3-derived identifier for a specific signed direct
/// message, used as the per-recipient tombstone key. 128 bits gives a
/// ~2^64 birthday bound - adequate against worst-case attacker-chosen
/// signature grinding (an attacker who can sign as themselves cannot
/// influence which token any *other* member's purge list contains, and
/// the recipient is the sole signer of their own purge list).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PurgeToken(pub [u8; 16]);

impl PurgeToken {
    /// Derive the tombstone for a sender signature.
    pub fn from_signature(signature: &Signature) -> Self {
        let digest = blake3::hash(signature.to_bytes().as_ref());
        let mut out = [0u8; 16];
        out.copy_from_slice(&digest.as_bytes()[..16]);
        PurgeToken(out)
    }
}

impl Serialize for PurgeToken {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for PurgeToken {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = <Vec<u8>>::deserialize(deserializer)?;
        let arr: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
            serde::de::Error::custom(format!(
                "expected 16-byte PurgeToken, got {} bytes",
                bytes.len()
            ))
        })?;
        Ok(PurgeToken(arr))
    }
}

// ---------------------------------------------------------------------------
// Signature byte wrapper (serde can't derive for `[u8; 64]` directly)
// ---------------------------------------------------------------------------

/// Newtype around a 64-byte Ed25519 signature, present only because
/// serde doesn't derive `Serialize`/`Deserialize` for `[u8; 64]`.
/// Used as a set key in [`DirectMessagesSummary`] for fast
/// "do we already have this signature?" lookups during delta
/// computation.
///
/// `Ord`/`PartialOrd` (over the raw 64 bytes) are required so the summary can
/// store these in a `BTreeSet` — a deterministic order is what keeps the
/// ciborium-serialized summary bytes identical across peers (see
/// [`DirectMessagesSummary`] and freenet/freenet-core#4857).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SignatureBytes(pub [u8; 64]);

impl Serialize for SignatureBytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for SignatureBytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = <Vec<u8>>::deserialize(deserializer)?;
        let arr: [u8; 64] = bytes.as_slice().try_into().map_err(|_| {
            serde::de::Error::custom(format!(
                "expected 64-byte Ed25519 signature, got {} bytes",
                bytes.len()
            ))
        })?;
        Ok(SignatureBytes(arr))
    }
}

// ---------------------------------------------------------------------------
// State shape
// ---------------------------------------------------------------------------

/// In-room direct-message sub-state. Wired into [`ChatRoomStateV1`] as
/// `direct_messages` with `#[serde(default)]` for back-compat with
/// pre-#230 encoded states.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectMessagesV1 {
    /// All sender-signed direct messages currently held.
    #[serde(default)]
    pub messages: Vec<AuthorizedDirectMessage>,

    /// Per-recipient purge envelopes (at most one per recipient).
    /// Stored as a sorted `Vec` (sorted by `recipient_id`) rather than
    /// `HashMap<MemberId, _>` because `MemberId` is a struct and
    /// `serde_json` rejects non-string map keys; see the bug-prevention
    /// pattern. `verify` enforces no-duplicate recipient_id.
    #[serde(default)]
    pub purges: Vec<AuthorizedRecipientPurges>,
}

/// A sender-signed direct message.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthorizedDirectMessage {
    pub message: DirectMessage,
    /// Sender's Ed25519 signature over the bytes produced by
    /// [`build_direct_message_signed_bytes`].
    pub sender_signature: Signature,
}

/// The signed payload of a direct message.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectMessage {
    /// Sender's [`MemberId`]. For owner-sent DMs, this is
    /// `MemberId::from(&parameters.owner)`.
    pub sender: MemberId,

    /// Recipient's [`MemberId`].
    pub recipient: MemberId,

    /// Unix timestamp (seconds since epoch). See [`check_dm_future_skew`].
    pub timestamp: u64,

    /// Opaque ciphertext, ECIES-encrypted to recipient's `member_vk`.
    pub ciphertext: Vec<u8>,
}

/// A recipient-signed purge envelope.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthorizedRecipientPurges {
    /// The recipient this envelope authorises purges for. MUST equal
    /// the `MemberId` derived from the signing key's `VerifyingKey`.
    pub recipient_id: MemberId,
    pub state: RecipientPurges,
    /// Recipient's Ed25519 signature over the bytes produced by
    /// [`build_recipient_purges_signed_bytes`].
    pub recipient_signature: Signature,
}

/// Recipient-controlled purge list.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecipientPurges {
    /// Monotonically increasing per-recipient. `0` is reserved as the
    /// "no purge envelope yet" sentinel: the first envelope MUST use
    /// `version >= 1`, and each subsequent envelope MUST use a strictly
    /// greater `version`. A version-bump MUST also be a superset of the
    /// previous list - un-purging is not allowed (`apply_delta` rejects
    /// any shrinking purge list).
    #[serde(default)]
    pub version: u64,

    /// BLAKE3-derived purge tokens of messages the recipient has
    /// purged. Once present, ANY incoming message whose token matches
    /// is dropped. Order within the list is canonical-sorted for
    /// signature determinism (see
    /// [`build_recipient_purges_signed_bytes`]).
    #[serde(default)]
    pub purged: Vec<PurgeToken>,
}

// ---------------------------------------------------------------------------
// Canonical signed-byte layouts
// ---------------------------------------------------------------------------

/// Build the bytes the sender signs for an [`AuthorizedDirectMessage`].
///
/// ```text
///     domain_tag                  ( 1 byte, = DOMAIN_TAG_MESSAGE)
///     sender_member_id_le_i64     ( 8 bytes)
///     recipient_member_id_le_i64  ( 8 bytes)
///     room_owner_vk               (32 bytes)
///     timestamp_le_u64            ( 8 bytes)
///     ciphertext_len_le_u32       ( 4 bytes)
///     ciphertext                  (variable)
/// ```
///
/// Canonical by construction: all fields fixed-length except the
/// trailing ciphertext, which is preceded by its u32 little-endian
/// length. The leading domain-separation tag prevents this signed
/// buffer from ever being byte-equal to a [`build_recipient_purges_signed_bytes`]
/// buffer regardless of crafted field lengths.
pub fn build_direct_message_signed_bytes(
    sender: MemberId,
    recipient: MemberId,
    room_owner_vk: &VerifyingKey,
    timestamp: u64,
    ciphertext: &[u8],
) -> Result<Vec<u8>, String> {
    let ct_len: u32 = ciphertext.len().try_into().map_err(|_| {
        format!(
            "DM ciphertext length {} does not fit in u32",
            ciphertext.len()
        )
    })?;
    let mut out = Vec::with_capacity(1 + 8 + 8 + 32 + 8 + 4 + ciphertext.len());
    out.push(DOMAIN_TAG_MESSAGE);
    out.extend_from_slice(&sender.0 .0.to_le_bytes());
    out.extend_from_slice(&recipient.0 .0.to_le_bytes());
    out.extend_from_slice(room_owner_vk.as_bytes());
    out.extend_from_slice(&timestamp.to_le_bytes());
    out.extend_from_slice(&ct_len.to_le_bytes());
    out.extend_from_slice(ciphertext);
    Ok(out)
}

/// Build the bytes the recipient signs for an
/// [`AuthorizedRecipientPurges`].
///
/// ```text
///     domain_tag                  ( 1 byte, = DOMAIN_TAG_PURGES)
///     recipient_member_id_le_i64  ( 8 bytes)
///     room_owner_vk               (32 bytes)
///     version_le_u64              ( 8 bytes)
///     purged_count_le_u32         ( 4 bytes)
///     purged                      (16 bytes per entry, in declared order)
/// ```
///
/// Each `purged` entry is encoded as 16 raw bytes (the [`PurgeToken`])
/// in the order they appear in [`RecipientPurges::purged`]. The list
/// should be sorted ascending for canonical comparison; signers SHOULD
/// sort before signing.
pub fn build_recipient_purges_signed_bytes(
    recipient: MemberId,
    room_owner_vk: &VerifyingKey,
    state: &RecipientPurges,
) -> Result<Vec<u8>, String> {
    let purged_count: u32 = state.purged.len().try_into().map_err(|_| {
        format!(
            "DM purge list length {} does not fit in u32",
            state.purged.len()
        )
    })?;
    let mut out = Vec::with_capacity(1 + 8 + 32 + 8 + 4 + state.purged.len() * 16);
    out.push(DOMAIN_TAG_PURGES);
    out.extend_from_slice(&recipient.0 .0.to_le_bytes());
    out.extend_from_slice(room_owner_vk.as_bytes());
    out.extend_from_slice(&state.version.to_le_bytes());
    out.extend_from_slice(&purged_count.to_le_bytes());
    for entry in &state.purged {
        out.extend_from_slice(&entry.0);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Helpers - sender / recipient signing
// ---------------------------------------------------------------------------

/// Sign a direct message. Sender's `MemberId` MUST match
/// `sender_sk.verifying_key()`.
pub fn sign_direct_message(
    sender_sk: &SigningKey,
    sender: MemberId,
    recipient: MemberId,
    room_owner_vk: &VerifyingKey,
    timestamp: u64,
    ciphertext: Vec<u8>,
) -> Result<AuthorizedDirectMessage, String> {
    debug_assert_eq!(
        sender,
        MemberId::from(&sender_sk.verifying_key()),
        "sender MemberId must derive from sender_sk"
    );
    if sender == recipient {
        return Err("DM sender and recipient must differ".to_string());
    }
    let bytes = build_direct_message_signed_bytes(
        sender,
        recipient,
        room_owner_vk,
        timestamp,
        &ciphertext,
    )?;
    let signature = sender_sk.sign(&bytes);
    Ok(AuthorizedDirectMessage {
        message: DirectMessage {
            sender,
            recipient,
            timestamp,
            ciphertext,
        },
        sender_signature: signature,
    })
}

/// Sign a recipient purge envelope. Recipient's `MemberId` MUST match
/// `recipient_sk.verifying_key()`. The purge list is canonicalised
/// (sorted + deduplicated) before signing.
pub fn sign_recipient_purges(
    recipient_sk: &SigningKey,
    recipient: MemberId,
    room_owner_vk: &VerifyingKey,
    mut state: RecipientPurges,
) -> Result<AuthorizedRecipientPurges, String> {
    debug_assert_eq!(
        recipient,
        MemberId::from(&recipient_sk.verifying_key()),
        "recipient MemberId must derive from recipient_sk"
    );
    state.purged.sort();
    state.purged.dedup();
    let bytes = build_recipient_purges_signed_bytes(recipient, room_owner_vk, &state)?;
    let signature = recipient_sk.sign(&bytes);
    Ok(AuthorizedRecipientPurges {
        recipient_id: recipient,
        state,
        recipient_signature: signature,
    })
}

/// Count messages currently stored from `sender` to `recipient`. Clients
/// call this before [`compose_direct_message`] so they can surface a
/// user-visible error instead of silently losing the message — the contract
/// `apply_delta` drops overflow without erroring (one over-eager sender
/// should not poison the merge for every peer; see
/// `direct_messages.rs::apply_delta` comments).
pub fn pair_message_count(
    state: &DirectMessagesV1,
    sender: MemberId,
    recipient: MemberId,
) -> usize {
    state
        .messages
        .iter()
        .filter(|m| m.message.sender == sender && m.message.recipient == recipient)
        .count()
}

/// Reject timestamps too far ahead of `now_secs`. Used at
/// message-construction / ingestion time; deliberately NOT called from
/// [`ComposableState::verify`] to avoid self-DoS on stored state.
pub fn check_dm_future_skew(timestamp: u64, now_secs: u64) -> Result<(), String> {
    if timestamp > now_secs.saturating_add(MAX_DM_FUTURE_SKEW_SECS) {
        Err(format!(
            "DM timestamp {} is more than {}s ahead of now ({})",
            timestamp, MAX_DM_FUTURE_SKEW_SECS, now_secs
        ))
    } else {
        Ok(())
    }
}

/// End-to-end helper: encrypt `body` to the recipient, sign as the sender,
/// and return a wire-ready [`AuthorizedDirectMessage`]. Both the UI and
/// `riverctl` call this so the bytes that hit `DirectMessagesV1::messages`
/// are byte-identical across clients.
///
/// Requires the `ecies-randomized` feature (delegate WASM never sends DMs,
/// only inspects them).
///
/// Caps enforced here so a client never tries to push state the contract
/// will silently drop:
/// * `body` is rejected when the resulting envelope exceeds
///   [`MAX_DM_CIPHERTEXT_BYTES`].
/// * `timestamp` is rejected if more than [`MAX_DM_FUTURE_SKEW_SECS`] ahead
///   of `now_secs` (the caller's view of wall-clock).
#[cfg(feature = "ecies-randomized")]
pub fn compose_direct_message(
    sender_sk: &SigningKey,
    recipient_vk: &VerifyingKey,
    room_owner_vk: &VerifyingKey,
    timestamp: u64,
    now_secs: u64,
    body: &[u8],
) -> Result<AuthorizedDirectMessage, String> {
    check_dm_future_skew(timestamp, now_secs)?;

    let sender = MemberId::from(&sender_sk.verifying_key());
    let recipient = MemberId::from(recipient_vk);
    if sender == recipient {
        return Err("DM sender and recipient must differ".to_string());
    }

    let envelope = crate::ecies::seal_dm_for_recipient(recipient_vk, body);
    if envelope.len() > MAX_DM_CIPHERTEXT_BYTES {
        return Err(format!(
            "DM body too large: envelope {} bytes exceeds cap {} (body {} bytes; {} bytes of crypto overhead)",
            envelope.len(),
            MAX_DM_CIPHERTEXT_BYTES,
            body.len(),
            envelope.len() - body.len()
        ));
    }

    sign_direct_message(
        sender_sk,
        sender,
        recipient,
        room_owner_vk,
        timestamp,
        envelope,
    )
}

/// Inverse of [`compose_direct_message`]: decrypt a DM's ciphertext back to
/// plaintext bytes using the recipient's signing key. Does NOT verify the
/// sender signature — call [`AuthorizedDirectMessage::verify_signature`]
/// separately when freshness matters.
///
/// Feature-gated on `ecies` because the wire-format decryption lives in
/// [`crate::ecies`], which is itself `#[cfg(feature = "ecies")]`. The
/// room-contract WASM does not enable `ecies` (it only validates signed
/// envelopes, never reads plaintext); making this unconditional would break
/// that build.
#[cfg(feature = "ecies")]
pub fn open_direct_message(
    recipient_sk: &SigningKey,
    msg: &AuthorizedDirectMessage,
) -> Result<Vec<u8>, String> {
    crate::ecies::unseal_dm_from_sender(recipient_sk, &msg.message.ciphertext)
}

/// Construct a fresh [`AuthorizedRecipientPurges`] that bumps the recipient's
/// purge envelope to `previous.version + 1` (or `1` if `previous` is `None`)
/// and unions in `new_tokens`. The combined list is canonicalised
/// (sorted + deduplicated) and rejected when it exceeds
/// [`MAX_PURGED_TOMBSTONES_PER_RECIPIENT`].
pub fn advance_recipient_purges(
    recipient_sk: &SigningKey,
    room_owner_vk: &VerifyingKey,
    previous: Option<&AuthorizedRecipientPurges>,
    new_tokens: impl IntoIterator<Item = PurgeToken>,
) -> Result<AuthorizedRecipientPurges, String> {
    let recipient = MemberId::from(&recipient_sk.verifying_key());
    if let Some(prev) = previous {
        if prev.recipient_id != recipient {
            return Err(format!(
                "advance_recipient_purges: previous envelope is for recipient {:?}, but signing key is for {:?}",
                prev.recipient_id, recipient
            ));
        }
    }

    let prev_version = previous.map(|p| p.state.version).unwrap_or(0);
    let next_version = prev_version
        .checked_add(1)
        .ok_or_else(|| "recipient purges version overflow".to_string())?;

    let mut combined: Vec<PurgeToken> =
        previous.map(|p| p.state.purged.clone()).unwrap_or_default();
    combined.extend(new_tokens);
    combined.sort();
    combined.dedup();

    if combined.len() > MAX_PURGED_TOMBSTONES_PER_RECIPIENT {
        return Err(format!(
            "recipient purge list would exceed cap: {} > {}",
            combined.len(),
            MAX_PURGED_TOMBSTONES_PER_RECIPIENT
        ));
    }

    sign_recipient_purges(
        recipient_sk,
        recipient,
        room_owner_vk,
        RecipientPurges {
            version: next_version,
            purged: combined,
        },
    )
}

// ---------------------------------------------------------------------------
// Verification helpers
// ---------------------------------------------------------------------------

impl AuthorizedDirectMessage {
    /// Verify the sender signature against the resolved sender
    /// verifying key.
    pub fn verify_signature(
        &self,
        sender_vk: &VerifyingKey,
        room_owner_vk: &VerifyingKey,
    ) -> Result<(), String> {
        let bytes = build_direct_message_signed_bytes(
            self.message.sender,
            self.message.recipient,
            room_owner_vk,
            self.message.timestamp,
            &self.message.ciphertext,
        )?;
        sender_vk
            .verify(&bytes, &self.sender_signature)
            .map_err(|e| format!("Invalid DM sender signature: {}", e))
    }

    /// BLAKE3-derived tombstone token for this signature; what the
    /// recipient records in [`RecipientPurges::purged`].
    pub fn purge_token(&self) -> PurgeToken {
        PurgeToken::from_signature(&self.sender_signature)
    }

    /// This message's position in the per-pair retention order — the key
    /// [`trim_pairs_to_cap`] prunes by, and the same `(timestamp, signature)`
    /// order [`sort_state`] uses within a pair. Must stay in step with both;
    /// see [`DmPairHorizon`].
    pub fn order_key(&self) -> DmOrderKey {
        DmOrderKey {
            timestamp: self.message.timestamp,
            signature: SignatureBytes(self.sender_signature.to_bytes()),
        }
    }
}

impl AuthorizedRecipientPurges {
    /// Verify the recipient signature against the resolved recipient
    /// verifying key.
    pub fn verify_signature(
        &self,
        recipient_vk: &VerifyingKey,
        room_owner_vk: &VerifyingKey,
    ) -> Result<(), String> {
        let bytes =
            build_recipient_purges_signed_bytes(self.recipient_id, room_owner_vk, &self.state)?;
        recipient_vk
            .verify(&bytes, &self.recipient_signature)
            .map_err(|e| format!("Invalid recipient purges signature: {}", e))
    }
}

// ---------------------------------------------------------------------------
// Banned-DM sweep (called from ChatRoomStateV1::post_apply_cleanup)
// ---------------------------------------------------------------------------

impl DirectMessagesV1 {
    /// Set of member IDs that appear as a sender or recipient of a DM — or as
    /// the recipient of a purge envelope — that would SURVIVE
    /// [`Self::sweep_after_membership_change`] called with the same arguments.
    ///
    /// Used by `ChatRoomStateV1::post_apply_cleanup` step 1 to keep DM
    /// participants AND purge-envelope holders in the active members list. The
    /// latter is required so a recipient's purge envelope is not swept along
    /// with the recipient as soon as they have purged their last DM (and have
    /// no recent room messages): dropping the envelope would re-enable a stale
    /// peer to re-merge the original signed DM, undermining the
    /// tombstone-as-block guarantee.
    ///
    /// IDEMPOTENCE (freenet/river#671): the filter is not optional. This used
    /// to be an unfiltered `active_participants()` walk over EVERY held DM,
    /// which made the step-1 exemption strictly weaker than the step-6 sweep
    /// that runs later in the same pass: a member whose only claim to retention
    /// was a DM with a banned or departed counterparty was exempted on pass 1,
    /// had that DM swept by step 6 of the same pass, and was pruned on pass 2 —
    /// so `cleanup(S) != cleanup(cleanup(S))`. Sharing
    /// [`dm_endpoint_is_live`] with the sweep makes exemption ⟺ retention.
    /// This is the same fix #411 round 4 applied to the banner exemption; see
    /// the IDEMPOTENCE note in `ChatRoomStateV1::post_apply_cleanup`.
    ///
    /// Not circular: a counted participant is inserted into `required_ids`, so
    /// it survives step 3, so step 6 sees both endpoints alive and keeps the DM.
    pub fn participants_of_surviving_dms(
        &self,
        owner_id: MemberId,
        active_member_ids: &HashSet<MemberId>,
        banned_ids: &HashSet<MemberId>,
    ) -> HashSet<MemberId> {
        let alive = |id: MemberId| dm_endpoint_is_live(id, owner_id, active_member_ids, banned_ids);
        let mut out = HashSet::with_capacity(self.messages.len() * 2 + self.purges.len());
        for m in &self.messages {
            if alive(m.message.sender) && alive(m.message.recipient) {
                out.insert(m.message.sender);
                out.insert(m.message.recipient);
            }
        }
        for p in &self.purges {
            if alive(p.recipient_id) {
                out.insert(p.recipient_id);
            }
        }
        out
    }

    /// The [`DmPairHorizon`] entries this peer publishes: one per ordered
    /// `(sender, recipient)` pair that has reached [`MAX_DM_MESSAGES_PER_PAIR`],
    /// carrying the oldest key that pair still holds.
    ///
    /// Computed as the MINIMUM held key rather than the exact post-trim cutoff,
    /// and only once the pair is at or over the cap, so it never over-states:
    /// a sender is never told to withhold a DM the pair would in fact have
    /// kept. Under-stating merely costs an extra round, and the horizon rises
    /// strictly each round, so the exchange still terminates.
    ///
    /// Returned sorted, because these bytes are part of what freenet-core
    /// byte-compares to decide staleness.
    pub fn pair_horizons(&self) -> Vec<DmPairHorizon> {
        let mut by_pair: HashMap<(MemberId, MemberId), (usize, DmOrderKey)> = HashMap::new();
        for m in &self.messages {
            let key = m.order_key();
            match by_pair.entry((m.message.sender, m.message.recipient)) {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let (count, oldest) = e.get_mut();
                    *count += 1;
                    if key < *oldest {
                        *oldest = key;
                    }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((1, key));
                }
            }
        }

        let mut horizons: Vec<DmPairHorizon> = by_pair
            .into_iter()
            .filter(|(_, (count, _))| *count >= MAX_DM_MESSAGES_PER_PAIR)
            .map(
                |((sender, recipient), (_, oldest_retained))| DmPairHorizon {
                    sender,
                    recipient,
                    oldest_retained,
                },
            )
            .collect();
        horizons.sort();
        horizons
    }

    /// The [`DmRetentionHorizon`] this peer publishes for the given global cap.
    ///
    /// Computed as the MINIMUM held key rather than the exact post-trim cutoff,
    /// and only once the peer is at or over the cap, so it never over-states —
    /// see [`DmRetentionHorizon`] for why that direction is the safe one.
    ///
    /// Does not assume `self.messages` is sorted, and could not rely on it if
    /// it were: `sort_state` orders by `(sender, recipient, timestamp,
    /// signature)`, so the room-wide oldest DM is NOT at index 0. `verify` does
    /// not enforce any ordering either, so a hand-built or hostile full-state
    /// PUT can arrive in any order. Taking the min is correct regardless.
    pub fn global_retention_horizon(&self, max_direct_messages: usize) -> DmRetentionHorizon {
        if max_direct_messages == 0 {
            return DmRetentionHorizon::Closed;
        }
        if self.messages.len() < max_direct_messages {
            return DmRetentionHorizon::Open;
        }
        match self.messages.iter().map(|m| m.order_key()).min() {
            Some(oldest) => DmRetentionHorizon::OldestRetained(oldest),
            // Unreachable: len >= max_direct_messages >= 1 means non-empty.
            // `Open` is the safe fallback (offers more, never withholds).
            None => DmRetentionHorizon::Open,
        }
    }

    /// Drop any DM whose sender or recipient is banned (`banned_ids`),
    /// or is not a current member of the room (`active_member_ids`,
    /// owner-implicit). Called by `ChatRoomStateV1::post_apply_cleanup`
    /// to keep `verify` stable after bans / member churn - see the
    /// module-level "Interaction with bans" section. Also drops purge
    /// envelopes belonging to non-members so the state doesn't carry
    /// signatures from former-members forever.
    pub fn sweep_after_membership_change(
        &mut self,
        owner_id: MemberId,
        active_member_ids: &HashSet<MemberId>,
        banned_ids: &HashSet<MemberId>,
    ) {
        let alive = |id: MemberId| dm_endpoint_is_live(id, owner_id, active_member_ids, banned_ids);
        self.messages
            .retain(|m| alive(m.message.sender) && alive(m.message.recipient));
        self.purges.retain(|p| alive(p.recipient_id));
    }
}

/// The member-id set [`DirectMessagesV1::sweep_after_membership_change`] wants,
/// from the by-id map `apply_delta` already built for [`resolve_member_vk`].
fn member_ids_of(members_by_id: &HashMap<MemberId, &AuthorizedMember>) -> HashSet<MemberId> {
    members_by_id.keys().copied().collect()
}

/// The enforced-ban set `post_apply_cleanup` step 0 will compute, derived here
/// from `parent_state` — freenet/river#675.
///
/// This is the SAME set, not an approximation, and that is structural rather
/// than lucky: the `#[composable]` macro applies fields in DECLARATION order,
/// so `configuration`, `bans`, `members` and `member_info` — every input to
/// step 0-cap and step 0 — are already final when the `direct_messages` field
/// applies, and no later field mutates a sibling. (That ordering is itself
/// pinned by `field_declaration_order_puts_members_before_direct_messages`.)
///
/// Nothing is re-implemented: the eviction ORDERING is
/// [`BansV1::enforce_user_ban_cap`], the one function step 0-cap also calls,
/// and the id derivation is [`MembersV1::banned_member_ids`], which step 0 also
/// calls. The only thing done twice is applying them to a COPY, because
/// `apply_delta` must not mutate its parent — and the copy is taken only when
/// the ban set is actually over cap.
///
/// # Why this exists
///
/// It is what lets the apply-time sweep use the FULL step-6 predicate instead
/// of a membership-only approximation. Without it, a **deputy-issued** ban left
/// its target a member through this field's apply — `MembersV1::apply_delta` is
/// handed an empty `MemberInfoV1`, so it can only enforce owner and ancestor
/// authority, and a deputy grant lives in `member_info.deputies` where it
/// cannot see it. The target's DMs were therefore still ranked by
/// [`trim_to_global_cap`] and only removed afterwards by step 6: the
/// freenet/river#671 data loss, reachable on the flow the Official room's
/// moderators actually use. Measured on that reproduction, five legitimate DMs
/// were destroyed per merge; with this, zero.
///
/// An earlier revision of this file asserted the enforced-ban set was "not
/// knowable here" and treated membership-only as forced. It was not knowable
/// only in the sense that nobody had computed it; the claim was wrong and is
/// recorded here so it is not re-derived.
pub(crate) fn enforced_ban_set_of(
    parent_state: &ChatRoomStateV1,
    parameters: &ChatRoomParametersV1,
) -> HashSet<MemberId> {
    let owner_id = parameters.owner_id();
    let max_bans = parent_state.configuration.configuration.max_user_bans;

    if parent_state.bans.0.len() <= max_bans {
        return parent_state.members.banned_member_ids(
            &parent_state.bans,
            &parent_state.member_info,
            parameters,
        );
    }
    let members_by_id = parent_state.members.members_by_member_id();
    let mut capped: Vec<AuthorizedUserBan> = parent_state.bans.0.clone();
    BansV1::enforce_user_ban_cap(
        &mut capped,
        max_bans,
        &members_by_id,
        &parent_state.member_info,
        owner_id,
        &parameters.owner,
    );
    parent_state
        .members
        .banned_member_ids(&BansV1(capped), &parent_state.member_info, parameters)
}

/// Whether a DM endpoint is still a live participant: the room owner (implicit,
/// never present in `members`), or a current member who is not enforced-banned.
///
/// The single definition of "this DM survives cleanup", shared by
/// [`DirectMessagesV1::sweep_after_membership_change`] (step 6, which deletes
/// the DMs it rejects) and [`DirectMessagesV1::participants_of_surviving_dms`]
/// (step 1, which exempts their participants from inactivity-prune). One
/// function rather than two matching copies is what makes exemption ⟺
/// retention hold by construction — the copies are exactly what drifted apart
/// in freenet/river#671.
fn dm_endpoint_is_live(
    id: MemberId,
    owner_id: MemberId,
    active_member_ids: &HashSet<MemberId>,
    banned_ids: &HashSet<MemberId>,
) -> bool {
    id == owner_id || (active_member_ids.contains(&id) && !banned_ids.contains(&id))
}

// ---------------------------------------------------------------------------
// ComposableState impl
// ---------------------------------------------------------------------------

impl ComposableState for DirectMessagesV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = DirectMessagesSummary;
    type Delta = DirectMessagesDelta;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        let owner_id = parameters.owner_id();
        let members_by_id = parent_state.members.members_by_member_id();

        // ---- purges: signature + cap + duplicate-recipient + version ----
        let mut seen_recipients: HashSet<MemberId> = HashSet::new();
        for purges in &self.purges {
            if !seen_recipients.insert(purges.recipient_id) {
                return Err(format!(
                    "DM purges: duplicate envelope for recipient {:?}",
                    purges.recipient_id
                ));
            }
            if purges.state.version == 0 {
                return Err(format!(
                    "DM purges for {:?}: version 0 is reserved as the absent sentinel",
                    purges.recipient_id
                ));
            }
            if purges.state.purged.len() > MAX_PURGED_TOMBSTONES_PER_RECIPIENT {
                return Err(format!(
                    "DM purges for {:?} exceed cap: {} > {}",
                    purges.recipient_id,
                    purges.state.purged.len(),
                    MAX_PURGED_TOMBSTONES_PER_RECIPIENT
                ));
            }
            let recipient_vk =
                resolve_member_vk(purges.recipient_id, owner_id, parameters, &members_by_id)
                    .ok_or_else(|| {
                        format!(
                            "DM purges: recipient {:?} is not a current member",
                            purges.recipient_id
                        )
                    })?;
            purges.verify_signature(&recipient_vk, &parameters.owner)?;
        }

        // Build per-recipient tombstone sets for O(1) lookup during the
        // message loop.
        let purges_by_recipient: HashMap<MemberId, HashSet<PurgeToken>> = self
            .purges
            .iter()
            .map(|p| (p.recipient_id, p.state.purged.iter().copied().collect()))
            .collect();

        // ---- messages: signature + cap + membership + tombstone ----
        //
        // Bans are NOT enforced here - see module-level "Interaction
        // with bans". Banned-participant DMs are removed by
        // `ChatRoomStateV1::post_apply_cleanup`, so `verify` stays
        // stable across ban-state changes.
        let mut per_pair: HashMap<(MemberId, MemberId), usize> = HashMap::new();
        for msg in &self.messages {
            if msg.message.ciphertext.len() > MAX_DM_CIPHERTEXT_BYTES {
                return Err(format!(
                    "DM ciphertext too large: {} > {}",
                    msg.message.ciphertext.len(),
                    MAX_DM_CIPHERTEXT_BYTES
                ));
            }

            if msg.message.sender == msg.message.recipient {
                return Err(format!(
                    "DM sender and recipient must differ ({:?})",
                    msg.message.sender
                ));
            }

            let sender_vk =
                resolve_member_vk(msg.message.sender, owner_id, parameters, &members_by_id)
                    .ok_or_else(|| {
                        format!("DM sender {:?} is not a current member", msg.message.sender)
                    })?;

            if resolve_member_vk(msg.message.recipient, owner_id, parameters, &members_by_id)
                .is_none()
            {
                return Err(format!(
                    "DM recipient {:?} is not a current member",
                    msg.message.recipient
                ));
            }

            msg.verify_signature(&sender_vk, &parameters.owner)?;

            // Tombstone check: if the recipient has purged this signature,
            // the message must not be present.
            if let Some(tombstones) = purges_by_recipient.get(&msg.message.recipient) {
                if tombstones.contains(&msg.purge_token()) {
                    return Err(format!(
                        "DM from {:?} to {:?} is present despite being purged",
                        msg.message.sender, msg.message.recipient
                    ));
                }
            }

            let count = per_pair
                .entry((msg.message.sender, msg.message.recipient))
                .or_insert(0);
            *count += 1;
            if *count > MAX_DM_MESSAGES_PER_PAIR {
                return Err(format!(
                    "DM pair ({:?} -> {:?}) exceeds cap: {} > {}",
                    msg.message.sender, msg.message.recipient, count, MAX_DM_MESSAGES_PER_PAIR
                ));
            }
        }

        Ok(())
    }

    /// NOTE: this `summarize` READS `parent_state`, for the global DM cap that
    /// sizes [`DirectMessagesSummary::global_horizon`] — the same dependency
    /// [`crate::room_state::message::MessagesV1::summarize`] has on
    /// `max_recent_messages`, and with the same requirement: callers MUST pass
    /// the SUMMARIZING peer's own state. Passing a cheap
    /// `ChatRoomStateV1::default()` sentinel reads the DEFAULT cap instead of
    /// the room's, understating the horizon and re-opening the resend loop.
    /// The DM-side pin is
    /// `dm_global_cap_test::whole_state_gossip_under_the_cap_converges_and_stays_verifiable`
    /// (the messages-side `merge_uses_room_state_as_parent_so_horizon_is_correct`
    /// would not catch a DM-specific regression).
    fn summarize(
        &self,
        parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        let message_signatures: BTreeSet<SignatureBytes> = self
            .messages
            .iter()
            .map(|m| SignatureBytes(m.sender_signature.to_bytes()))
            .collect();

        let purge_versions: Vec<(MemberId, u64)> = {
            let mut v: Vec<(MemberId, u64)> = self
                .purges
                .iter()
                .map(|p| (p.recipient_id, p.state.version))
                .collect();
            v.sort_by_key(|(k, _)| *k);
            v
        };

        DirectMessagesSummary {
            message_signatures,
            purge_versions,
            pair_horizons: self.pair_horizons(),
            global_horizon: self.global_retention_horizon(
                parent_state
                    .configuration
                    .configuration
                    .effective_max_direct_messages(),
            ),
        }
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        let prior_versions: HashMap<MemberId, u64> =
            old_state_summary.purge_versions.iter().copied().collect();

        // A DM the receiver's per-pair cap would discard the instant it applied
        // it must never be offered, or the pair loops forever re-sending it.
        // See [`DmPairHorizon`].
        let horizons: HashMap<(MemberId, MemberId), &DmOrderKey> = old_state_summary
            .pair_horizons
            .iter()
            .map(|h| ((h.sender, h.recipient), &h.oldest_retained))
            .collect();

        let new_messages: Vec<AuthorizedDirectMessage> = self
            .messages
            .iter()
            .filter(|m| {
                !old_state_summary
                    .message_signatures
                    .contains(&SignatureBytes(m.sender_signature.to_bytes()))
            })
            .filter(
                |m| match horizons.get(&(m.message.sender, m.message.recipient)) {
                    // The receiver's pair is below the cap: it keeps anything.
                    None => true,
                    Some(oldest) => m.order_key() > **oldest,
                },
            )
            // Same rule again on the GLOBAL axis: a DM can sit comfortably
            // inside its own pair's window and still be the oldest DM in the
            // room, so clearing `pair_horizons` is not enough to know the
            // receiver will keep it. See [`DmRetentionHorizon`].
            .filter(|m| match &old_state_summary.global_horizon {
                DmRetentionHorizon::Open => true,
                DmRetentionHorizon::OldestRetained(oldest) => m.order_key() > *oldest,
                DmRetentionHorizon::Closed => false,
            })
            .cloned()
            .collect();

        let advanced_purges: Vec<AuthorizedRecipientPurges> = self
            .purges
            .iter()
            .filter_map(|p| {
                let prior = prior_versions.get(&p.recipient_id).copied().unwrap_or(0);
                if p.state.version > prior {
                    Some(p.clone())
                } else {
                    None
                }
            })
            .collect();

        if new_messages.is_empty() && advanced_purges.is_empty() {
            None
        } else {
            Some(DirectMessagesDelta {
                new_messages,
                advanced_purges,
            })
        }
    }

    fn apply_delta(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        let max_direct_messages = parent_state
            .configuration
            .configuration
            .effective_max_direct_messages();

        let owner_id = parameters.owner_id();
        let members_by_id = parent_state.members.members_by_member_id();

        let Some(delta) = delta else {
            // Sweep unresolvable endpoints, then enforce the caps and
            // re-sort, even when THIS field has no delta. What that converges:
            // a state that ARRIVED unnormalised — over-cap, or holding DMs
            // whose endpoints `members` has since dropped — by a path that
            // skips this function, namely a full-state PUT or the #292
            // migration PUT. Such a PUT is how the state got IN; it is NOT how
            // this arm is REACHED. This arm runs only when the top-level delta
            // is present and the `direct_messages` sub-delta is None; a bare
            // top-level None short-circuits every field. See the test.
            self.sweep_after_membership_change(
                owner_id,
                &member_ids_of(&members_by_id),
                &enforced_ban_set_of(parent_state, parameters),
            );
            enforce_caps_and_sort(self, max_direct_messages);
            return Ok(());
        };

        // ---- 1. Apply purge advances first ----
        //
        // The recipient is the sole signer of their own envelope, so
        // strict-monotonic `version` is the entire ordering rule. A
        // duplicate-version with different content is a protocol error
        // (the same signer wouldn't sign two different envelopes at
        // the same version). Each new version's purge list MUST be a
        // superset of the previous version's list (no un-purging).
        for advance in &delta.advanced_purges {
            if advance.state.version == 0 {
                return Err(format!(
                    "DM purges for {:?}: version 0 is reserved as the absent sentinel",
                    advance.recipient_id
                ));
            }
            if advance.state.purged.len() > MAX_PURGED_TOMBSTONES_PER_RECIPIENT {
                return Err(format!(
                    "DM purges for {:?} exceed cap: {} > {}",
                    advance.recipient_id,
                    advance.state.purged.len(),
                    MAX_PURGED_TOMBSTONES_PER_RECIPIENT
                ));
            }
            let recipient_vk =
                match resolve_member_vk(advance.recipient_id, owner_id, parameters, &members_by_id)
                {
                    Some(vk) => vk,
                    // Recipient is either not yet a member on this peer
                    // (member-add and purge envelope arriving in
                    // separate deltas in the wrong order) or no longer
                    // a member at all. Silent-drop; a subsequent
                    // summary-driven sync will deliver the envelope
                    // once the member entry is present.
                    None => continue,
                };
            advance.verify_signature(&recipient_vk, &parameters.owner)?;

            let pos = self
                .purges
                .iter()
                .position(|p| p.recipient_id == advance.recipient_id);
            match pos {
                Some(idx) => {
                    let current = &self.purges[idx];
                    if current.state.version > advance.state.version {
                        continue; // already up to date
                    }
                    if current.state.version == advance.state.version {
                        // Same-version-different-content is a recipient
                        // signing bug (a multi-device user who didn't
                        // coordinate version numbers, or a malicious
                        // client). Drop the incoming envelope silently
                        // - first-seen wins. Returning Err here would
                        // poison the whole delta merge, taking
                        // unrelated `new_messages` and other recipients'
                        // `advanced_purges` with it. The recipient is
                        // expected to bump the version to converge.
                        continue;
                    }
                    // Monotonic-content: new must be a superset of old.
                    let current_set: HashSet<PurgeToken> =
                        current.state.purged.iter().copied().collect();
                    let advance_set: HashSet<PurgeToken> =
                        advance.state.purged.iter().copied().collect();
                    if !current_set.is_subset(&advance_set) {
                        // Recipient is trying to un-purge tokens by
                        // shrinking the list across a version bump.
                        // Silent-drop the malformed envelope rather
                        // than failing the whole delta.
                        continue;
                    }
                    self.purges[idx] = advance.clone();
                }
                None => {
                    self.purges.push(advance.clone());
                }
            }
        }

        // ---- 2. Apply new messages, gated by the up-to-date purges ----
        let mut existing_sigs: HashSet<SignatureBytes> = self
            .messages
            .iter()
            .map(|m| SignatureBytes(m.sender_signature.to_bytes()))
            .collect();

        let purges_index: HashMap<MemberId, HashSet<PurgeToken>> = self
            .purges
            .iter()
            .map(|p| (p.recipient_id, p.state.purged.iter().copied().collect()))
            .collect();

        for msg in &delta.new_messages {
            if msg.message.ciphertext.len() > MAX_DM_CIPHERTEXT_BYTES {
                continue; // silently drop oversized messages
            }

            if msg.message.sender == msg.message.recipient {
                continue; // silently drop self-DMs
            }

            // Dedup against current state - and against earlier
            // messages already accepted in this same delta.
            let sig = SignatureBytes(msg.sender_signature.to_bytes());
            if existing_sigs.contains(&sig) {
                continue;
            }

            let sender_vk =
                match resolve_member_vk(msg.message.sender, owner_id, parameters, &members_by_id) {
                    Some(vk) => vk,
                    None => continue, // sender no longer a member - silently drop
                };

            if resolve_member_vk(msg.message.recipient, owner_id, parameters, &members_by_id)
                .is_none()
            {
                continue; // recipient no longer a member - silently drop
            }

            if msg.verify_signature(&sender_vk, &parameters.owner).is_err() {
                continue; // bad signature - silently drop
            }

            // Tombstone gate.
            if let Some(tombstones) = purges_index.get(&msg.message.recipient) {
                if tombstones.contains(&msg.purge_token()) {
                    continue;
                }
            }

            // The per-pair cap is NOT applied here. It used to be, as
            // first-come-wins ("already at the cap? drop the arrival"), and
            // that was wrong twice over:
            //
            //  * Convergence: which messages a peer ends up holding depended on
            //    ARRIVAL ORDER, so two peers could sit at the cap with
            //    different sets, each re-offering what the other discards, and
            //    `delta` never emptied. See [`DmPairHorizon`].
            //  * Behaviour: once a pair filled up, every later DM from that
            //    sender was silently dropped forever.
            //
            // Accept everything that passes authorisation and let
            // `trim_pairs_to_cap` below keep the NEWEST `MAX_DM_MESSAGES_PER_PAIR`
            // — a deterministic function of the union, so peers converge.
            existing_sigs.insert(sig);
            self.messages.push(msg.clone());
        }

        // ---- 3. Drop any existing messages that are now tombstoned ----
        // This handles the case where a purge envelope arrives in the
        // same delta as (or after) a message-bearing delta that already
        // installed the message.
        let purges_after: HashMap<MemberId, HashSet<PurgeToken>> = self
            .purges
            .iter()
            .map(|p| (p.recipient_id, p.state.purged.iter().copied().collect()))
            .collect();
        self.messages.retain(|m| {
            !purges_after
                .get(&m.message.recipient)
                .is_some_and(|set| set.contains(&m.purge_token()))
        });

        // ---- 4. Drop held DMs whose endpoints are no longer members ----
        // MUST run before the caps: a DM the room can no longer keep must not
        // be ranked against ones it can, or it wins a slot and evicts a
        // legitimate message before `post_apply_cleanup` step 6 deletes it
        // anyway (freenet/river#671). Applies to the held set the same
        // membership rule step 2 above applies to arrivals.
        self.sweep_after_membership_change(
            owner_id,
            &member_ids_of(&members_by_id),
            &enforced_ban_set_of(parent_state, parameters),
        );

        // ---- 5. Enforce the caps, newest-first, then sort ----
        enforce_caps_and_sort(self, max_direct_messages);

        Ok(())
    }
}

/// Apply both retention caps and restore the canonical stored order.
///
/// The per-pair trim runs FIRST, and the order is load-bearing for RETENTION,
/// not for legality: either order leaves every pair legal, because whichever
/// trim runs last only shrinks the set further. What the swapped order loses is
/// messages. Running the global trim first spends the global budget on DMs that
/// the per-pair trim is about to discard anyway, so the surviving set can end up
/// well BELOW the global cap while DMs that would have fitted were dropped —
/// e.g. one busy pair holding the room's newest 150 DMs under a global cap of
/// 200 yields 200 retained pair-first but only 150 global-first. Pinned by
/// `pair_trim_runs_before_the_global_trim_so_the_budget_is_not_wasted`.
///
/// Every step is a pure function of the held set, so the composition is too:
/// two peers reaching the same union converge to the same state, and running
/// this twice changes nothing after the first pass (idempotent).
fn enforce_caps_and_sort(s: &mut DirectMessagesV1, max_direct_messages: usize) {
    dedup_by_signature(s);
    trim_pairs_to_cap(s);
    trim_to_global_cap(s, max_direct_messages);
    sort_state(s);
}

/// Drop duplicate entries, keeping the first occurrence of each signature.
///
/// `apply_delta` already dedupes on insert, so a peer that only ever merges
/// deltas never holds duplicates. `verify` does NOT reject them, though — it
/// counts duplicates toward the per-pair cap but has no distinct-signature
/// check — so a hand-built or hostile full-state PUT can carry them, and
/// `validate_state` is `verify`.
///
/// That matters because both trims key off [`DmOrderKey`], and duplicate
/// entries carry IDENTICAL keys (same timestamp, same signature). Without this,
/// [`trim_to_global_cap`]'s cutoff can land on a repeated key, in which case
/// `retain(>= cutoff)` drops nothing and the peer sits permanently over the cap
/// — the exact failure the cap exists to prevent. Deduping first makes the
/// "keys are unique" premise TRUE rather than assumed.
///
/// Removal-only, so it cannot make a verifying state fail `verify`, and it is a
/// pure function of the held set, so it preserves convergence.
fn dedup_by_signature(s: &mut DirectMessagesV1) {
    let mut seen: HashSet<SignatureBytes> = HashSet::with_capacity(s.messages.len());
    s.messages
        .retain(|m| seen.insert(SignatureBytes(m.sender_signature.to_bytes())));
}

/// Keep only the newest [`MAX_DM_MESSAGES_PER_PAIR`] messages in each ordered
/// `(sender, recipient)` pair, by [`AuthorizedDirectMessage::order_key`].
///
/// A pure function of the held set, so every peer that ends up with the same
/// union trims to the same result regardless of the order the messages arrived
/// in. That determinism is what makes the pair converge; the paired
/// [`DmPairHorizon`] in the summary is what stops a sender re-offering the
/// entries this drops.
fn trim_pairs_to_cap(s: &mut DirectMessagesV1) {
    // No single pair can exceed the cap while the whole set is within it.
    if s.messages.len() <= MAX_DM_MESSAGES_PER_PAIR {
        return;
    }

    let mut by_pair: HashMap<(MemberId, MemberId), Vec<(DmOrderKey, SignatureBytes)>> =
        HashMap::new();
    for m in &s.messages {
        by_pair
            .entry((m.message.sender, m.message.recipient))
            .or_default()
            .push((m.order_key(), SignatureBytes(m.sender_signature.to_bytes())));
    }

    let mut dropped: HashSet<SignatureBytes> = HashSet::new();
    for entries in by_pair.values_mut() {
        if entries.len() <= MAX_DM_MESSAGES_PER_PAIR {
            continue;
        }
        // Ascending, so the oldest — the ones that go — are at the front.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let excess = entries.len() - MAX_DM_MESSAGES_PER_PAIR;
        dropped.extend(entries.iter().take(excess).map(|(_, sig)| *sig));
    }

    if !dropped.is_empty() {
        s.messages
            .retain(|m| !dropped.contains(&SignatureBytes(m.sender_signature.to_bytes())));
    }
}

/// Keep only the newest `max_direct_messages` messages across the WHOLE set, by
/// [`AuthorizedDirectMessage::order_key`] — the global counterpart of
/// [`trim_pairs_to_cap`], added for freenet/river#519.
///
/// Ordering by `(timestamp, signature)` room-wide is a strict total order
/// (signatures are unique per message), so "newest N" is unambiguous. Like the
/// per-pair trim this is a pure function of the held set, so every peer that
/// ends up with the same union trims to the same result regardless of arrival
/// order; the paired [`DmRetentionHorizon`] in the summary is what stops a
/// sender re-offering the entries this drops.
///
/// Runs AFTER [`trim_pairs_to_cap`], so the pair cap can never be violated by
/// the global trim keeping a message the pair cap had already discarded.
fn trim_to_global_cap(s: &mut DirectMessagesV1, max_direct_messages: usize) {
    if s.messages.len() <= max_direct_messages {
        return;
    }
    if max_direct_messages == 0 {
        s.messages.clear();
        return;
    }

    // Select the cutoff by ranking keys rather than sorting `messages` itself:
    // `sort_state` owns the stored order, and reordering here would silently
    // couple the two.
    let mut keys: Vec<DmOrderKey> = s.messages.iter().map(|m| m.order_key()).collect();
    // `select_nth_unstable` puts the element that WOULD be at this index in
    // sorted order there, with everything smaller before it — exactly the
    // "drop the oldest `excess`" boundary, in O(n).
    let excess = keys.len() - max_direct_messages;
    keys.select_nth_unstable(excess);
    let cutoff = keys[excess].clone();

    // Strictly-below-cutoff goes; the cutoff key itself and everything above it
    // stays. Keys are unique — `enforce_caps_and_sort` runs `dedup_by_signature`
    // first, and a signature is unique per message — so this keeps exactly
    // `max_direct_messages`. Without that dedup a repeated cutoff key would
    // make this drop nothing, leaving the peer permanently over cap.
    s.messages.retain(|m| m.order_key() >= cutoff);
}

fn sort_state(s: &mut DirectMessagesV1) {
    s.messages.sort_by(|a, b| {
        a.message
            .sender
            .cmp(&b.message.sender)
            .then(a.message.recipient.cmp(&b.message.recipient))
            .then(a.message.timestamp.cmp(&b.message.timestamp))
            .then(
                a.sender_signature
                    .to_bytes()
                    .cmp(&b.sender_signature.to_bytes()),
            )
    });
    s.purges.sort_by_key(|p| p.recipient_id);
}

// ---------------------------------------------------------------------------
// Summary / Delta
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectMessagesSummary {
    /// Raw Ed25519 signatures of messages already held locally.
    ///
    /// `BTreeSet` (not `HashSet`) so the ciborium-serialized summary bytes are
    /// deterministic: freenet-core byte-compares `summarize_state` output for
    /// staleness, and a `HashSet` iterates in a per-process-random order,
    /// making two identical DM sets summarize to different bytes → spurious
    /// anti-entropy heals. See `.claude/rules/contract-summary-determinism.md`
    /// and freenet/freenet-core#4857.
    #[serde(default)]
    pub message_signatures: BTreeSet<SignatureBytes>,

    /// Per-recipient purge-envelope version known locally. Stored as a
    /// sorted `Vec` (not `HashMap`) so the type round-trips through
    /// `serde_json` - `MemberId` is a struct and `serde_json` rejects
    /// it as a map key.
    #[serde(default)]
    pub purge_versions: Vec<(MemberId, u64)>,

    /// One entry per ordered `(sender, recipient)` pair that has reached
    /// [`MAX_DM_MESSAGES_PER_PAIR`], carrying the oldest message that pair
    /// still holds. See [`DmPairHorizon`] for why this is here.
    ///
    /// Sorted (by the whole tuple) and stored as a `Vec` for the same two
    /// reasons as `purge_versions`: canonical bytes for freenet-core's
    /// summary comparison, and `serde_json` compatibility.
    #[serde(default)]
    pub pair_horizons: Vec<DmPairHorizon>,

    /// The whole-set counterpart of `pair_horizons`, for the GLOBAL cap
    /// (`Configuration::effective_max_direct_messages`). See
    /// [`DmRetentionHorizon`].
    ///
    /// `#[serde(default)]` yields [`DmRetentionHorizon::Open`], which is the
    /// safe direction: a peer whose summary predates this field is treated as
    /// accepting everything, so nothing is silently withheld from it.
    #[serde(default)]
    pub global_horizon: DmRetentionHorizon,
}

/// How much appetite a peer has for older direct messages GLOBALLY, published
/// in [`DirectMessagesSummary`] so a sender never offers a DM the receiver
/// would discard the instant it applied it.
///
/// # Why this exists, separately from [`DmPairHorizon`]
///
/// [`DmPairHorizon`] solves the identical problem one ordered pair at a time,
/// for the per-pair cap [`MAX_DM_MESSAGES_PER_PAIR`]. The global cap
/// introduced for freenet/river#519 prunes across ALL pairs, so it makes the
/// merge non-monotonic along an axis no per-pair horizon can describe: a DM
/// can be well inside its own pair's window and still be the oldest DM in the
/// room. Without this second horizon, `delta` re-offers exactly those DMs on
/// every fan-out, the receiver re-prunes them, neither summary changes, and
/// the pair loops forever — the same failure that drove the 2026-07-25
/// bandwidth incident, at up to [`MAX_DM_CIPHERTEXT_BYTES`] per message.
///
/// # Why it terminates
///
/// [`DmRetentionHorizon::OldestRetained`] is the smallest [`DmOrderKey`] the
/// peer currently holds, published only once it is AT the global cap. A sender
/// offers only strictly-greater keys. A peer BELOW the cap publishes `Open` and
/// discards nothing globally, so its signature set only grows.
///
/// Note the global horizon does NOT necessarily move on every accepted DM, and
/// the argument must not claim it does: when the arrival's own pair is at
/// [`MAX_DM_MESSAGES_PER_PAIR`], `trim_pairs_to_cap` runs first, drops that
/// pair's oldest, and returns the set to exactly the cap — so
/// [`trim_to_global_cap`] early-returns and this horizon is unchanged.
///
/// Termination comes from the two horizons TOGETHER, via the invariant that
/// every message either trim drops is strictly below at least one horizon the
/// peer publishes immediately afterwards. So an accepted DM always advances
/// something: either the set grew, or the pair trim dropped that pair's oldest
/// (that pair's horizon rose), or the global trim dropped the room's oldest
/// (this horizon rose). A dropped message is never re-offered, because it now
/// sits below a published horizon. Every exchange therefore grows a bounded set
/// or strictly advances a bounded key, so there are no cycles.
///
/// Deliberately conservative in the same direction as
/// [`crate::room_state::message::RetentionHorizon`]: publishing the oldest
/// HELD key rather than the exact post-merge cutoff can cost one extra round,
/// whereas over-stating would silently withhold DMs the peer would have kept.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum DmRetentionHorizon {
    /// The peer holds fewer than the global cap; it retains anything.
    #[default]
    Open,
    /// The peer is at (or over) the global cap and holds nothing ordering
    /// before this key. Anything at or below it is discarded on arrival.
    OldestRetained(DmOrderKey),
    /// The cap is `0`: the peer retains no direct messages at all.
    ///
    /// `AuthorizedConfigurationV1::apply_delta` rejects a zero cap, but
    /// `verify` does not, so an owner-signed zero can still arrive on the
    /// full-state path. Represented explicitly rather than folded into
    /// `OldestRetained` so the sender suppresses the delta instead of looping
    /// against a peer that keeps nothing.
    Closed,
}

/// The retention horizon for one ordered `(sender, recipient)` pair.
///
/// # Why this exists
///
/// [`DirectMessagesV1::apply_delta`] caps each ordered pair at
/// [`MAX_DM_MESSAGES_PER_PAIR`], which makes the merge non-monotonic in exactly
/// the way [`crate::room_state::message::RetentionHorizon`] documents for room
/// messages. Without a horizon, `delta` is a pure signature-set difference: a
/// peer whose window for the pair differs from its neighbour's re-offers DMs
/// the neighbour discards, on every fan-out, forever. At up to 32 KiB of
/// ciphertext each that is a heavy loop.
///
/// `oldest_retained` is the smallest key the pair currently holds, published
/// only once the pair is at capacity. A sender offers only strictly-greater
/// keys; applying one pushes the pair over the cap, so the trim drops at least
/// the horizon message itself and the horizon strictly increases. A pair below
/// capacity publishes no entry at all and accepts anything.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct DmPairHorizon {
    pub sender: MemberId,
    pub recipient: MemberId,
    pub oldest_retained: DmOrderKey,
}

/// Retention order for direct messages within one pair: `(timestamp,
/// signature)`, matching [`sort_state`]'s within-pair ordering. The signature
/// breaks timestamp ties deterministically.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct DmOrderKey {
    pub timestamp: u64,
    pub signature: SignatureBytes,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectMessagesDelta {
    #[serde(default)]
    pub new_messages: Vec<AuthorizedDirectMessage>,

    #[serde(default)]
    pub advanced_purges: Vec<AuthorizedRecipientPurges>,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Resolve a [`MemberId`] to its `VerifyingKey`. The owner is treated
/// as an implicit member: their key lives in `parameters.owner`, not
/// in `parent_state.members`.
fn resolve_member_vk(
    id: MemberId,
    owner_id: MemberId,
    parameters: &ChatRoomParametersV1,
    members_by_id: &HashMap<MemberId, &AuthorizedMember>,
) -> Option<VerifyingKey> {
    if id == owner_id {
        Some(parameters.owner)
    } else {
        members_by_id.get(&id).map(|m| m.member.member_vk)
    }
}

#[cfg(test)]
mod tests {
    // Unit tests for this module live in
    // `common/tests/direct_messages_test.rs` so they exercise the
    // public API the same way downstream consumers will.
}
