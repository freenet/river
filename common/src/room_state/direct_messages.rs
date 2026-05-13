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
//! - [`MAX_DM_MESSAGES_PER_PAIR`]: per (sender, recipient) ordered pair.
//! - [`MAX_DM_CIPHERTEXT_BYTES`]: per-message ciphertext size cap.
//! - [`MAX_PURGED_TOMBSTONES_PER_RECIPIENT`]: cap on per-recipient
//!   purge-list length.
//! - [`MAX_DM_FUTURE_SKEW_SECS`]: maximum permitted future-skew when
//!   accepting a fresh message (verifiable via
//!   [`check_dm_future_skew`]). Not enforced inside `verify` (would be
//!   self-DoS for already-stored state).

use crate::room_state::member::{AuthorizedMember, MemberId};
use crate::room_state::ChatRoomParametersV1;
use crate::ChatRoomStateV1;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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
/// Used as a hash-table key in [`DirectMessagesSummary`] for fast
/// "do we already have this signature?" lookups during delta
/// computation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    /// Set of member IDs that appear as a sender or recipient of any
    /// currently-held DM, OR as the recipient of any currently-held
    /// purge envelope. Used by `ChatRoomStateV1::post_apply_cleanup` to
    /// keep DM participants AND purge-envelope holders in the active
    /// members list. The latter is required so a recipient's purge
    /// envelope is not swept along with the recipient as soon as they
    /// have purged their last DM (and have no recent room messages):
    /// dropping the envelope would re-enable a stale peer to re-merge
    /// the original signed DM, undermining the tombstone-as-block
    /// guarantee.
    pub fn active_participants(&self) -> HashSet<MemberId> {
        let mut out = HashSet::with_capacity(self.messages.len() * 2 + self.purges.len());
        for m in &self.messages {
            out.insert(m.message.sender);
            out.insert(m.message.recipient);
        }
        for p in &self.purges {
            out.insert(p.recipient_id);
        }
        out
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
        let alive = |id: MemberId| -> bool {
            id == owner_id || (active_member_ids.contains(&id) && !banned_ids.contains(&id))
        };
        self.messages
            .retain(|m| alive(m.message.sender) && alive(m.message.recipient));
        self.purges.retain(|p| alive(p.recipient_id));
    }
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

    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        let message_signatures: HashSet<SignatureBytes> = self
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

        let new_messages: Vec<AuthorizedDirectMessage> = self
            .messages
            .iter()
            .filter(|m| {
                !old_state_summary
                    .message_signatures
                    .contains(&SignatureBytes(m.sender_signature.to_bytes()))
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
        let Some(delta) = delta else {
            // Even when no delta arrived, re-sort for deterministic
            // ordering (cheap, ensures verify-time invariant).
            sort_state(self);
            return Ok(());
        };

        let owner_id = parameters.owner_id();
        let members_by_id = parent_state.members.members_by_member_id();

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
        let mut per_pair_existing: HashMap<(MemberId, MemberId), usize> = HashMap::new();
        for m in &self.messages {
            *per_pair_existing
                .entry((m.message.sender, m.message.recipient))
                .or_insert(0) += 1;
        }

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

            // Per-pair cap - drop overflow rather than failing the
            // whole delta (one over-eager sender shouldn't poison the
            // merge for every peer).
            let pair_key = (msg.message.sender, msg.message.recipient);
            let pair_count = per_pair_existing.entry(pair_key).or_insert(0);
            if *pair_count >= MAX_DM_MESSAGES_PER_PAIR {
                continue;
            }
            *pair_count += 1;

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

        // ---- 4. Deterministic ordering for CRDT convergence ----
        sort_state(self);

        Ok(())
    }
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
    #[serde(default)]
    pub message_signatures: HashSet<SignatureBytes>,

    /// Per-recipient purge-envelope version known locally. Stored as a
    /// sorted `Vec` (not `HashMap`) so the type round-trips through
    /// `serde_json` - `MemberId` is a struct and `serde_json` rejects
    /// it as a map key.
    #[serde(default)]
    pub purge_versions: Vec<(MemberId, u64)>,
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
