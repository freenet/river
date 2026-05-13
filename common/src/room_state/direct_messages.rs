//! In-room direct messages (#230 Phase 1).
//!
//! End-to-end-encrypted DMs between two members of the same room,
//! carried inside `ChatRoomStateV1`. Replaces the reverted inbox-contract
//! approach (PR #234 → reverted in #238) — instead of a separate per-pair
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
//! - [`DirectMessagesV1::purges`][]: per-recipient
//!   [`AuthorizedRecipientPurges`] tombstone envelopes. Each recipient
//!   maintains a single, monotonically-versioned list of truncated
//!   `fast_hash(sender_signature) as u32` entries identifying messages
//!   they've purged. The recipient is the sole signer of their own
//!   purge envelope; concurrent updates are resolved by strict-monotonic
//!   `version`.
//!
//! # Authorisation model
//!
//! Every piece of state is cryptographically authorised at insertion:
//!
//! 1. Each [`AuthorizedDirectMessage`] carries a sender signature over
//!    canonical bytes (see [`build_direct_message_signed_bytes`]) that
//!    bind `sender`, `recipient`, `room_owner_vk`, `timestamp`, and
//!    `ciphertext`. The signature is verified against the sender's
//!    resolved `member_vk` (looked up in `parent_state.members`).
//!
//! 2. Each [`AuthorizedRecipientPurges`] carries a recipient signature
//!    over canonical bytes (see [`build_recipient_purges_signed_bytes`])
//!    that bind `recipient`, `room_owner_vk`, `version`, and the purge
//!    list. Verified against the recipient's resolved `member_vk`.
//!
//! 3. Both sender and recipient MUST be current members of the room
//!    and MUST NOT be in `parent_state.bans`. The owner is treated as
//!    an implicit member (their key is in `parameters.owner`).
//!
//! # Tombstone-as-block semantics
//!
//! Once a recipient signs a purge envelope listing
//! `fast_hash(sender_signature) as u32`, ANY incoming message whose
//! signature hashes to the same u32 is dropped on merge — this matches
//! how `BansV1` prevents re-adding banned members. So even if a peer
//! with stale state tries to re-merge the purged message back, the
//! current `purges` state blocks it.
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
use freenet_scaffold::util::fast_hash;
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

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

    /// Per-recipient purge envelopes. A recipient signs ONE envelope
    /// containing the cumulative set of purged-message hashes; later
    /// versions strictly replace earlier ones.
    #[serde(default)]
    pub purges: HashMap<MemberId, AuthorizedRecipientPurges>,
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
///
/// `recipient_id` is stored explicitly (rather than inferred from the
/// parent `HashMap<MemberId, _>` key) so the signed bytes can bind to
/// the recipient identity directly. The parent map key MUST equal
/// `recipient_id`; `verify` enforces this.
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
    /// greater `version`.
    #[serde(default)]
    pub version: u64,

    /// Truncated `fast_hash(sender_signature) as u32` of messages the
    /// recipient has purged. Once present, ANY incoming message whose
    /// signature hashes to one of these u32s is dropped. The u32
    /// truncation accepts a low false-positive rate (the recipient can
    /// always re-request from the sender out-of-band), and only the
    /// recipient writes this list, so there is no adversarial
    /// collision attack.
    #[serde(default)]
    pub purged: Vec<u32>,
}

// ---------------------------------------------------------------------------
// Canonical signed-byte layouts
// ---------------------------------------------------------------------------

/// Build the bytes the sender signs for an [`AuthorizedDirectMessage`].
///
/// ```text
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
/// length. No serializer dependency on the signed side; the signature
/// commits to this exact byte layout.
pub fn build_direct_message_signed_bytes(
    sender: MemberId,
    recipient: MemberId,
    room_owner_vk: &VerifyingKey,
    timestamp: u64,
    ciphertext: &[u8],
) -> Vec<u8> {
    let ct_len: u32 = ciphertext
        .len()
        .try_into()
        .expect("ciphertext length must fit in u32");
    let mut out = Vec::with_capacity(8 + 8 + 32 + 8 + 4 + ciphertext.len());
    out.extend_from_slice(&sender.0 .0.to_le_bytes());
    out.extend_from_slice(&recipient.0 .0.to_le_bytes());
    out.extend_from_slice(room_owner_vk.as_bytes());
    out.extend_from_slice(&timestamp.to_le_bytes());
    out.extend_from_slice(&ct_len.to_le_bytes());
    out.extend_from_slice(ciphertext);
    out
}

/// Build the bytes the recipient signs for an
/// [`AuthorizedRecipientPurges`].
///
/// ```text
///     recipient_member_id_le_i64  ( 8 bytes)
///     room_owner_vk               (32 bytes)
///     version_le_u64              ( 8 bytes)
///     purged_count_le_u32         ( 4 bytes)
///     purged                      (4 bytes per entry, in declared order)
/// ```
///
/// Each `purged` entry is encoded as 4 LE bytes (`u32`) in the order
/// they appear in [`RecipientPurges::purged`]. Adding a future field
/// appends new bytes; old envelopes remain valid because their
/// signature covered an older byte layout. Field reordering or removal
/// is a wire-format break.
pub fn build_recipient_purges_signed_bytes(
    recipient: MemberId,
    room_owner_vk: &VerifyingKey,
    state: &RecipientPurges,
) -> Vec<u8> {
    let purged_count: u32 = state
        .purged
        .len()
        .try_into()
        .expect("purge count must fit in u32");
    let mut out = Vec::with_capacity(8 + 32 + 8 + 4 + state.purged.len() * 4);
    out.extend_from_slice(&recipient.0 .0.to_le_bytes());
    out.extend_from_slice(room_owner_vk.as_bytes());
    out.extend_from_slice(&state.version.to_le_bytes());
    out.extend_from_slice(&purged_count.to_le_bytes());
    for entry in &state.purged {
        out.extend_from_slice(&entry.to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// Helpers — sender / recipient signing
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
) -> AuthorizedDirectMessage {
    debug_assert_eq!(
        sender,
        MemberId::from(&sender_sk.verifying_key()),
        "sender MemberId must derive from sender_sk"
    );
    let bytes =
        build_direct_message_signed_bytes(sender, recipient, room_owner_vk, timestamp, &ciphertext);
    let signature = sender_sk.sign(&bytes);
    AuthorizedDirectMessage {
        message: DirectMessage {
            sender,
            recipient,
            timestamp,
            ciphertext,
        },
        sender_signature: signature,
    }
}

/// Sign a recipient purge envelope. Recipient's `MemberId` MUST match
/// `recipient_sk.verifying_key()`.
pub fn sign_recipient_purges(
    recipient_sk: &SigningKey,
    recipient: MemberId,
    room_owner_vk: &VerifyingKey,
    state: RecipientPurges,
) -> AuthorizedRecipientPurges {
    debug_assert_eq!(
        recipient,
        MemberId::from(&recipient_sk.verifying_key()),
        "recipient MemberId must derive from recipient_sk"
    );
    let bytes = build_recipient_purges_signed_bytes(recipient, room_owner_vk, &state);
    let signature = recipient_sk.sign(&bytes);
    AuthorizedRecipientPurges {
        recipient_id: recipient,
        state,
        recipient_signature: signature,
    }
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
        );
        sender_vk
            .verify(&bytes, &self.sender_signature)
            .map_err(|e| format!("Invalid DM sender signature: {}", e))
    }

    /// `fast_hash(sender_signature) as u32` — the value the recipient
    /// records in [`RecipientPurges::purged`].
    pub fn purge_token(&self) -> u32 {
        fast_hash(self.sender_signature.to_bytes().as_ref()).0 as u32
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
            build_recipient_purges_signed_bytes(self.recipient_id, room_owner_vk, &self.state);
        recipient_vk
            .verify(&bytes, &self.recipient_signature)
            .map_err(|e| format!("Invalid recipient purges signature: {}", e))
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
        let banned_ids: HashSet<MemberId> = parent_state
            .bans
            .0
            .iter()
            .map(|b| b.ban.banned_user)
            .collect();

        // ---- purges: signature + cap + key/value consistency ----
        for (key_id, purges) in &self.purges {
            if *key_id != purges.recipient_id {
                return Err(format!(
                    "DM purges: HashMap key {:?} does not match signed recipient_id {:?}",
                    key_id, purges.recipient_id
                ));
            }
            if purges.state.purged.len() > MAX_PURGED_TOMBSTONES_PER_RECIPIENT {
                return Err(format!(
                    "DM purges for {:?} exceed cap: {} > {}",
                    key_id,
                    purges.state.purged.len(),
                    MAX_PURGED_TOMBSTONES_PER_RECIPIENT
                ));
            }
            let recipient_vk = resolve_member_vk(*key_id, owner_id, parameters, &members_by_id)
                .ok_or_else(|| {
                    format!("DM purges: recipient {:?} is not a current member", key_id)
                })?;
            purges.verify_signature(&recipient_vk, &parameters.owner)?;
        }

        // ---- messages: signature + cap + membership + ban + tombstone ----
        let mut per_pair: HashMap<(MemberId, MemberId), usize> = HashMap::new();
        for msg in &self.messages {
            if msg.message.ciphertext.len() > MAX_DM_CIPHERTEXT_BYTES {
                return Err(format!(
                    "DM ciphertext too large: {} > {}",
                    msg.message.ciphertext.len(),
                    MAX_DM_CIPHERTEXT_BYTES
                ));
            }

            // Sender + recipient must be current room members (or owner).
            let sender_vk =
                resolve_member_vk(msg.message.sender, owner_id, parameters, &members_by_id)
                    .ok_or_else(|| {
                        format!("DM sender {:?} is not a current member", msg.message.sender)
                    })?;

            if banned_ids.contains(&msg.message.sender) {
                return Err(format!("DM sender {:?} is banned", msg.message.sender));
            }

            if resolve_member_vk(msg.message.recipient, owner_id, parameters, &members_by_id)
                .is_none()
            {
                return Err(format!(
                    "DM recipient {:?} is not a current member",
                    msg.message.recipient
                ));
            }

            if banned_ids.contains(&msg.message.recipient) {
                return Err(format!(
                    "DM recipient {:?} is banned",
                    msg.message.recipient
                ));
            }

            msg.verify_signature(&sender_vk, &parameters.owner)?;

            // Tombstone check: if the recipient has purged this signature,
            // the message must not be present.
            if let Some(purges) = self.purges.get(&msg.message.recipient) {
                if purges.state.purged.contains(&msg.purge_token()) {
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

        let purge_versions: HashMap<MemberId, u64> = self
            .purges
            .iter()
            .map(|(k, v)| (*k, v.state.version))
            .collect();

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
            .filter_map(|(k, v)| {
                let prior = old_state_summary
                    .purge_versions
                    .get(k)
                    .copied()
                    .unwrap_or(0);
                if v.state.version > prior {
                    Some(v.clone())
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
            return Ok(());
        };

        let owner_id = parameters.owner_id();
        let members_by_id = parent_state.members.members_by_member_id();
        let banned_ids: HashSet<MemberId> = parent_state
            .bans
            .0
            .iter()
            .map(|b| b.ban.banned_user)
            .collect();

        // ---- 1. Apply purge advances first ----
        //
        // The recipient is the sole signer of their own envelope, so
        // strict-monotonic `version` is the entire ordering rule. A
        // duplicate-version with different content is a protocol error
        // (the same signer wouldn't sign two different envelopes at
        // the same version).
        for advance in &delta.advanced_purges {
            if advance.state.version == 0 {
                return Err("DM purges: version 0 is reserved as the absent sentinel".to_string());
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
                resolve_member_vk(advance.recipient_id, owner_id, parameters, &members_by_id)
                    .ok_or_else(|| {
                        format!(
                            "DM purges: recipient {:?} is not a current member",
                            advance.recipient_id
                        )
                    })?;
            advance.verify_signature(&recipient_vk, &parameters.owner)?;

            match self.purges.get(&advance.recipient_id) {
                Some(current) if current.state.version >= advance.state.version => {
                    if current.state.version == advance.state.version
                        && current.state != advance.state
                    {
                        return Err(format!(
                            "DM purges: conflicting envelopes at version {} for {:?}",
                            advance.state.version, advance.recipient_id
                        ));
                    }
                    // strictly-greater not satisfied — skip (already up to date)
                }
                _ => {
                    self.purges.insert(advance.recipient_id, advance.clone());
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

        let existing_sigs: HashSet<SignatureBytes> = self
            .messages
            .iter()
            .map(|m| SignatureBytes(m.sender_signature.to_bytes()))
            .collect();

        for msg in &delta.new_messages {
            if msg.message.ciphertext.len() > MAX_DM_CIPHERTEXT_BYTES {
                return Err(format!(
                    "DM ciphertext too large: {} > {}",
                    msg.message.ciphertext.len(),
                    MAX_DM_CIPHERTEXT_BYTES
                ));
            }

            // Dedup against current state.
            if existing_sigs.contains(&SignatureBytes(msg.sender_signature.to_bytes())) {
                continue;
            }

            let sender_vk =
                match resolve_member_vk(msg.message.sender, owner_id, parameters, &members_by_id) {
                    Some(vk) => vk,
                    None => continue, // sender no longer a member — silently drop
                };

            if banned_ids.contains(&msg.message.sender) {
                continue; // sender banned — silently drop
            }

            if resolve_member_vk(msg.message.recipient, owner_id, parameters, &members_by_id)
                .is_none()
            {
                continue; // recipient no longer a member — silently drop
            }

            if banned_ids.contains(&msg.message.recipient) {
                continue; // recipient banned — silently drop
            }

            msg.verify_signature(&sender_vk, &parameters.owner)?;

            // Tombstone gate.
            if let Some(purges) = self.purges.get(&msg.message.recipient) {
                if purges.state.purged.contains(&msg.purge_token()) {
                    continue;
                }
            }

            // Per-pair cap.
            let pair_key = (msg.message.sender, msg.message.recipient);
            let pair_count = per_pair_existing.entry(pair_key).or_insert(0);
            if *pair_count >= MAX_DM_MESSAGES_PER_PAIR {
                return Err(format!(
                    "DM pair ({:?} -> {:?}) would exceed cap of {}",
                    msg.message.sender, msg.message.recipient, MAX_DM_MESSAGES_PER_PAIR
                ));
            }
            *pair_count += 1;

            self.messages.push(msg.clone());
        }

        // ---- 3. Drop any existing messages that are now tombstoned ----
        // This handles the case where a purge envelope arrives in the
        // same delta as (or after) a message-bearing delta that already
        // installed the message.
        self.messages
            .retain(|m| match self.purges.get(&m.message.recipient) {
                Some(p) => !p.state.purged.contains(&m.purge_token()),
                None => true,
            });

        // ---- 4. Deterministic ordering for CRDT convergence ----
        self.messages.sort_by(|a, b| {
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

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Summary / Delta
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectMessagesSummary {
    /// Raw Ed25519 signatures of messages already held locally.
    #[serde(default)]
    pub message_signatures: HashSet<SignatureBytes>,

    /// Per-recipient purge-envelope version known locally; 0 if absent.
    #[serde(default)]
    pub purge_versions: HashMap<MemberId, u64>,
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
