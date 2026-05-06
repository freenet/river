//! River inbox contract — Phase 1 (v4).
//!
//! One inbox per `(recipient, room)` pair, keyed deterministically by
//! [`InboxParams`]. Members of the room push end-to-end-encrypted
//! messages here; the recipient's chat delegate decrypts them locally
//! and curates the inbox via a recipient-signed
//! [`AuthorizedRecipientState`].
//!
//! # v4 design — self-contained membership proofs
//!
//! Earlier drafts tied each inbox message to the room's contract via
//! the freenet-stdlib related-contracts mechanism, which required the
//! inbox WASM to know the room-contract WASM hash (a coupling that
//! turned every room-contract bump into an inbox-contract bump).
//!
//! v4 eliminates that coupling. Each member-sent [`InboxMessage`]
//! carries a self-contained [`MembershipProof`]: the sender's own
//! [`AuthorizedMember`] plus the invitation chain back to (but not
//! including) the room owner. The contract verifies the chain locally
//! against `params.room_owner_vk` — the room owner's verifying key,
//! which doubles as the room identifier. No related-contracts request
//! is issued from `validate_state`.
//!
//! Trade-off: the inbox can't see the room's live ban list, so a
//! banned member with a valid pre-ban [`AuthorizedMember`] can keep
//! sending until the recipient purges them. River's recipient-signed
//! purge primitive ([`InboxDelta::UpdateRecipientState`]) handles
//! cleanup; the inbox is single-recipient, so the threat model is
//! "one user spamming one user" rather than "one user spamming a
//! room".
//!
//! # Authorisation model
//!
//! Every piece of state is cryptographically authorised:
//!
//! 1. Each [`InboxMessage`] carries a signature by its sender. The
//!    signed bytes (see [`build_signed_payload_bytes`]) bind sender
//!    identity, recipient identity, room identity (via
//!    `room_owner_vk`), the membership proof (via
//!    [`compute_proof_hash`]), timestamp, and the ciphertext — so the
//!    same payload cannot be replayed against a different inbox, and
//!    the same signature cannot be paired with a different
//!    `member_proof` value on different peers (which would break the
//!    byte-equivalence convergence required by Freenet's CRDT model).
//!    The contract additionally enforces
//!    `member_proof.sender_authorized.member.id() == InboxMessage::sender`.
//! 2. Owner-sent messages are recognised by
//!    `sender == fast_hash(params.room_owner_vk)`; in that case
//!    `member_proof` MUST be `None` and the signature is verified
//!    against `params.room_owner_vk` directly.
//! 3. Member-sent messages carry `member_proof = Some(...)`. The
//!    chain is verified against `params.room_owner_vk` per the rules
//!    in [`chain`]. The sender's `VerifyingKey` is resolved from
//!    `member_proof.sender_authorized.member.member_vk` and
//!    `InboxMessage::signature` is verified against it.
//! 4. The recipient-controlled state is wrapped in a single signed
//!    envelope ([`AuthorizedRecipientState`]) following River's
//!    `Configuration` pattern. The wrapping signature implicitly
//!    authorises the entire payload, including the order of any
//!    contained `Vec`s.
//!
//! # Time / replay protection
//!
//! - **Future skew**: messages timestamped more than
//!   [`MAX_FUTURE_SKEW_SECS`] ahead of `freenet_stdlib::time::now()`
//!   are rejected. Applied only to *incoming* messages in
//!   `update_state`, never to messages already stored in
//!   `validate_state` — the latter would be a self-DoS.
//! - **No past-skew bound**: stored messages remain valid
//!   indefinitely. Replay protection comes from (a) signature dedup
//!   against current messages and recipient-maintained tombstones,
//!   and (b) the monotonic `version` on [`RecipientState`].
//!
//! # Bounds
//!
//! - [`MAX_INBOX_MESSAGES`] caps total queue size.
//! - [`MAX_CIPHERTEXT_BYTES`] caps per-message ciphertext.
//! - [`MAX_PURGED_TOMBSTONES`] caps the recipient-maintained
//!   tombstone list.
//! - [`MAX_CHAIN_DEPTH`] caps invitation-chain length per message.
//!
//! # Recipient-signed state replacement
//!
//! Update-deltas come in two flavours:
//!
//! - [`InboxDelta::AppendMessages`] adds new sender-signed messages.
//! - [`InboxDelta::UpdateRecipientState`] replaces the
//!   recipient-controlled state with a strictly higher `version`. Any
//!   messages in `messages` whose `fast_hash(signature) as u32`
//!   appears in the new `purged` list are removed atomically.

use std::collections::HashSet;

use ciborium::{de::from_reader, ser::into_writer};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use freenet_scaffold::util::{fast_hash, FastHash};
use freenet_stdlib::prelude::*;
use river_core::room_state::member::{AuthorizedMember, MemberId};
use serde::{Deserialize, Serialize};

pub mod chain;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Maximum number of messages an inbox is permitted to hold.
pub const MAX_INBOX_MESSAGES: usize = 1000;

/// Maximum permitted size of any single ciphertext, in bytes.
pub const MAX_CIPHERTEXT_BYTES: usize = 32_768;

/// Maximum tombstone entries the recipient may keep. The recipient is
/// responsible for FIFO discipline (eviction when full).
pub const MAX_PURGED_TOMBSTONES: usize = 1000;

/// How far ahead of `time::now()` an *incoming* message timestamp may
/// be (seconds). Applied in `update_state` only — `validate_state`
/// does NOT enforce it on already-stored messages.
///
/// There is intentionally **no past-skew bound**. A past-skew check
/// on stored state would be a self-DoS: every inbox would
/// spontaneously become invalid once its oldest message aged past the
/// bound.
pub const MAX_FUTURE_SKEW_SECS: u64 = 5 * 60;

/// Maximum invitation-chain depth in a [`MembershipProof`]. Counts the
/// total number of [`AuthorizedMember`] entries (`sender_authorized`
/// plus everything in `invitation_chain`). Bounds the
/// signature-verification work per message — at most this many
/// Ed25519 verifications run for a single proof.
///
/// Real River invitation chains are typically 1–3 levels (see the
/// crate-level doc on the v4 design). 8 is generous headroom for
/// realistic deployments while keeping per-message verification cost
/// bounded and predictable. If a deployment ever needs deeper chains
/// this becomes a coordinated wire-format change (the contract WASM
/// hash and therefore the contract id changes).
pub const MAX_CHAIN_DEPTH: usize = 8;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Parameters defining a unique inbox contract instance.
///
/// The contract instance id is `BLAKE3(WASM_hash || cbor(params))`.
/// Each `(recipient, room)` pair gets its own inbox; River uses
/// per-room signing keys, so `room_owner_vk` uniquely identifies a
/// room.
///
/// Note: neither `recipient_vk` nor `room_owner_vk` carries
/// `#[serde(default)]`. Parameters drive the contract instance id —
/// substituting a default value would mean a different contract
/// altogether, so silently defaulting on missing input would be
/// nonsensical. Decoding must fail loudly if either field is absent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxParams {
    /// Recipient's room-scoped verifying key. Identifies which member
    /// of the room owns this inbox.
    pub recipient_vk: VerifyingKey,

    /// Room owner's verifying key. Identifies the room. Used to
    /// verify owner-sent messages directly AND as the root of every
    /// member's chain in [`MembershipProof`].
    pub room_owner_vk: VerifyingKey,
}

/// Inbox state: sender-signed messages plus recipient-controlled
/// metadata.
///
/// Container fields carry `#[serde(default)]` for backwards
/// compatibility with future field additions; required *contents*
/// (e.g. signatures inside [`InboxMessage`]) are intentionally
/// non-defaulted so a malformed encoding fails to deserialise rather
/// than producing a zero-valued "valid-shape" record.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Inbox {
    /// Each [`InboxMessage`] is independently authenticated by its
    /// sender's signature plus a self-contained membership proof.
    #[serde(default)]
    pub messages: Vec<InboxMessage>,

    /// Recipient-controlled state. `None` until the recipient first
    /// signs an [`InboxDelta::UpdateRecipientState`] — sender-initiated
    /// PUTs leave this empty.
    #[serde(default)]
    pub recipient_state: Option<AuthorizedRecipientState>,
}

/// One signed inbox message.
///
/// Two flavours of authentication, distinguished by `member_proof`:
///
/// - **Owner-sent**: `sender == fast_hash(params.room_owner_vk)` and
///   `member_proof == None`. Signature verified against
///   `params.room_owner_vk` directly.
///
/// - **Member-sent**: `member_proof == Some(...)`. Chain verified
///   locally against `params.room_owner_vk`; sender's actual
///   `VerifyingKey` resolved from
///   `member_proof.sender_authorized.member.member_vk`; signature
///   verified against that resolved VK.
///
/// In both cases the signed payload commits to `room_owner_vk`, so a
/// member of room A cannot produce a message that validates in
/// `inbox(_, room_B)`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxMessage {
    /// Sender's identity. For owner-sent messages this is
    /// `fast_hash(params.room_owner_vk)`; for member-sent messages,
    /// this is `member_proof.sender_authorized.member.id()`.
    pub sender: MemberId,

    /// Unix timestamp (seconds). Rejected if more than
    /// [`MAX_FUTURE_SKEW_SECS`] ahead of the host's clock at
    /// validation time. No past-skew bound.
    pub timestamp: u64,

    /// Opaque ciphertext, ECIES-encrypted to `recipient_vk`.
    pub ciphertext: Vec<u8>,

    /// Sender's Ed25519 signature over the bytes produced by
    /// [`build_signed_payload_bytes`]. Binds sender + recipient +
    /// room owner + `proof_hash` + timestamp + ciphertext. The
    /// `proof_hash` field commits the signature to a specific
    /// `member_proof` value (or to `None` for owner-sent messages),
    /// preventing a peer from pairing the same signature with a
    /// different valid proof on different replicas.
    pub signature: Signature,

    /// `None` for owner-sent messages
    /// (`sender == fast_hash(params.room_owner_vk)`).
    /// `Some` for member-sent messages: contains the sender's
    /// [`AuthorizedMember`] and the invitation chain back to the
    /// room owner. Self-contained; no related-contracts lookup is
    /// needed.
    pub member_proof: Option<MembershipProof>,
}

/// Self-contained proof that the sender is (or was) a member of the
/// room identified by `params.room_owner_vk`.
///
/// The contract verifies (see [`chain::verify_membership_proof`]):
///
/// 1. Total chain depth (`1 + invitation_chain.len()`) is at most
///    [`MAX_CHAIN_DEPTH`].
/// 2. Each [`AuthorizedMember`] in the chain has a valid signature
///    against the next link's `member_vk` (or against
///    `params.room_owner_vk` for the chain root).
/// 3. Chain links are consistent: each member's `invited_by` matches
///    the next link's `member.id()`. The chain root's `invited_by`
///    must equal `MemberId::from(&params.room_owner_vk)`.
/// 4. `sender_authorized.member.id()` matches the
///    [`InboxMessage::sender`] field.
/// 5. `sender_authorized.member.member_vk` is the
///    [`VerifyingKey`] used to verify
///    [`InboxMessage::signature`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MembershipProof {
    /// The sender's own [`AuthorizedMember`].
    pub sender_authorized: AuthorizedMember,

    /// Path of [`AuthorizedMember`] entries from sender's immediate
    /// inviter back to (but not including) the room owner. Empty if
    /// the sender was directly invited by the owner. Length is
    /// at most `MAX_CHAIN_DEPTH - 1`.
    #[serde(default)]
    pub invitation_chain: Vec<AuthorizedMember>,
}

/// Recipient-signed envelope around the recipient-controlled fields.
/// The signature implicitly authorises the entire `state` payload —
/// including the order of any contained lists. Following River's
/// `Configuration` pattern.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthorizedRecipientState {
    pub state: RecipientState,

    /// Recipient's Ed25519 signature over the canonical bytes
    /// produced by [`build_recipient_state_signed_bytes`].
    pub signature: Signature,
}

/// Recipient-controlled fields. Internal layout is authorised by the
/// wrapping signature, not per-field. New fields may be added with
/// `#[serde(default)]` for backwards compatibility, and the
/// signed-bytes builder must be extended to include them at the tail.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecipientState {
    /// Monotonically increasing per-recipient. Each new
    /// [`AuthorizedRecipientState`] must have `version` strictly
    /// greater than the existing one's. Prevents replay of older
    /// recipient-signed states.
    ///
    /// `#[serde(default)]` is safe here despite `version` being a
    /// security-relevant field. The downstream strict-monotonicity
    /// gate (`new.version > current.version` in
    /// `apply_update_recipient_state` / `apply_full_state`) catches
    /// any accidental default-0 incoming once a prior recipient_state
    /// exists. The only path that lets a default-0 through is the
    /// genuinely-initial case where no prior recipient_state has been
    /// set, which is benign. Removing the default would force initial
    /// PUTs to explicitly include `version: 0` and silently break
    /// callers that rely on the default; keeping it is the right
    /// trade-off.
    #[serde(default)]
    pub version: u64,

    /// FIFO of truncated `fast_hash(signature) as u32` of purged
    /// messages. New `AppendMessages` whose hash matches an entry
    /// here are rejected. The recipient maintains the list (length
    /// bounded by [`MAX_PURGED_TOMBSTONES`]; recipient evicts oldest
    /// when adding new). Truncation to u32 is acceptable because
    /// false positives are bounded — the recipient never sees the
    /// message and can re-request from the sender — and only the
    /// recipient can write to this list, so there is no adversarial
    /// collision attack.
    #[serde(default)]
    pub purged: Vec<u32>,
    // Future fields go here, each with `#[serde(default)]`. Examples:
    //   pub blocked_senders: Vec<MemberId>,
    //   pub max_messages_override: Option<u16>,
    //   pub last_read_timestamp: u64,
    //
    // When adding a field, also extend
    // `build_recipient_state_signed_bytes` to commit it at the tail.
    // Old signed states stay valid because their signature covered an
    // older tail-truncated layout.
}

/// Compact summary used by peers to compute deltas.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxSummary {
    /// 64-byte Ed25519 signatures of messages already present
    /// locally.
    #[serde(default)]
    pub message_signatures: Vec<Vec<u8>>,

    /// Version of the `recipient_state` currently held, or 0 if
    /// absent.
    #[serde(default)]
    pub recipient_state_version: u64,
}

/// A delta sent through `update_state`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum InboxDelta {
    /// Add new sender-signed messages. Each is independently validated
    /// (signature, future-skew, ciphertext size, membership proof,
    /// and not in the current tombstone set). Cap enforcement happens
    /// against running totals — intermediate over-cap states are
    /// rejected immediately rather than after the whole batch is
    /// merged.
    AppendMessages(Vec<InboxMessage>),

    /// Replace the recipient-controlled state. The new
    /// [`AuthorizedRecipientState`]'s `version` must be strictly
    /// greater than the existing one's. After this delta, any message
    /// in `messages` whose hash appears in the new `purged` list is
    /// removed.
    UpdateRecipientState(AuthorizedRecipientState),
}

// ---------------------------------------------------------------------------
// Manual byte layouts that bind signatures
// ---------------------------------------------------------------------------

/// Build the bytes the sender signs.
///
/// ```text
///     sender_member_id_le_i64    ( 8 bytes)   <-- binds sender identity
///     recipient_vk               (32 bytes)   <-- binds inbox; prevents cross-inbox replay
///     room_owner_vk              (32 bytes)   <-- binds room
///     proof_hash                 (32 bytes)   <-- binds member_proof identity (or `None`)
///     timestamp_le_u64           ( 8 bytes)
///     ciphertext_len_le_u32      ( 4 bytes)
///     ciphertext                 (variable)
/// ```
///
/// Canonical by construction: all fields fixed-length except
/// trailing ciphertext, with explicit length prefix. No
/// truncation/extension ambiguity.
///
/// `proof_hash = compute_proof_hash(&member_proof)`. Including it in
/// the signed payload commits the signature to a specific
/// `member_proof` value: a peer cannot pair the same signature with
/// two different valid proofs on different replicas without
/// re-signing, which preserves byte-equivalence convergence under
/// Freenet's CRDT model. (For owner-sent messages, `member_proof` is
/// always `None` and `proof_hash` is the constant
/// `blake3(cbor(None::<MembershipProof>))`.)
pub fn build_signed_payload_bytes(
    sender: MemberId,
    recipient_vk: &VerifyingKey,
    room_owner_vk: &VerifyingKey,
    proof_hash: &[u8; 32],
    timestamp: u64,
    ciphertext: &[u8],
) -> Vec<u8> {
    let ct_len: u32 = ciphertext
        .len()
        .try_into()
        .expect("ciphertext length must fit in u32");
    let mut out = Vec::with_capacity(8 + 32 + 32 + 32 + 8 + 4 + ciphertext.len());
    out.extend_from_slice(&sender.0 .0.to_le_bytes());
    out.extend_from_slice(recipient_vk.as_bytes());
    out.extend_from_slice(room_owner_vk.as_bytes());
    out.extend_from_slice(proof_hash);
    out.extend_from_slice(&timestamp.to_le_bytes());
    out.extend_from_slice(&ct_len.to_le_bytes());
    out.extend_from_slice(ciphertext);
    out
}

/// Compute the canonical hash of a `member_proof` field for inclusion
/// in [`build_signed_payload_bytes`].
///
/// The hash is over the deterministic ciborium serialisation of the
/// `Option<MembershipProof>`. ciborium's serialiser is deterministic
/// for fixed input structures (it always produces the same bytes for
/// the same value), so both the signer and verifier compute the same
/// hash from the same proof. For owner-sent messages
/// (`member_proof: None`), this resolves to the fixed constant
/// `blake3(cbor(None::<MembershipProof>))`, computed identically on
/// both sides.
pub fn compute_proof_hash(member_proof: &Option<MembershipProof>) -> [u8; 32] {
    let mut buf = Vec::new();
    into_writer(member_proof, &mut buf)
        .expect("ciborium serialisation of Option<MembershipProof> is infallible");
    let h = blake3::hash(&buf);
    *h.as_bytes()
}

/// Build the bytes the recipient signs to authorise a
/// [`RecipientState`].
///
/// ```text
///     recipient_vk               (32 bytes)   <-- binds to this inbox
///     version_le_u64             ( 8 bytes)
///     purged_count_le_u32        ( 4 bytes)
///     purged                     (4 bytes per entry × count, in order)
///     [future fields appended in declared order]
/// ```
///
/// Adding a future field appends new bytes; old
/// `AuthorizedRecipientState`s remain valid because their signature
/// covered an older byte layout. Field reordering or removal is a
/// wire-format break.
pub fn build_recipient_state_signed_bytes(
    recipient_vk: &VerifyingKey,
    state: &RecipientState,
) -> Vec<u8> {
    let purged_count: u32 = state
        .purged
        .len()
        .try_into()
        .expect("purged count must fit in u32");
    let mut out = Vec::with_capacity(32 + 8 + 4 + state.purged.len() * 4);
    out.extend_from_slice(recipient_vk.as_bytes());
    out.extend_from_slice(&state.version.to_le_bytes());
    out.extend_from_slice(&purged_count.to_le_bytes());
    for id in &state.purged {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// Helpers and signers (also useful as a sender-path SDK)
// ---------------------------------------------------------------------------

impl InboxMessage {
    /// Verify `self.signature` is a valid Ed25519 signature by
    /// `actual_sender_vk` over [`build_signed_payload_bytes`]. The
    /// caller must have first resolved `self.sender` (a `MemberId`)
    /// to the corresponding `VerifyingKey` — either the owner's VK
    /// directly (owner-sent) or from
    /// `self.member_proof.sender_authorized.member.member_vk`
    /// (member-sent).
    ///
    /// `proof_hash` is computed from `self.member_proof` and included
    /// in the signed payload, so substituting a different valid proof
    /// post-signing breaks the signature.
    pub fn verify_signature(
        &self,
        actual_sender_vk: &VerifyingKey,
        recipient_vk: &VerifyingKey,
        room_owner_vk: &VerifyingKey,
    ) -> Result<(), String> {
        let proof_hash = compute_proof_hash(&self.member_proof);
        let payload = build_signed_payload_bytes(
            self.sender,
            recipient_vk,
            room_owner_vk,
            &proof_hash,
            self.timestamp,
            &self.ciphertext,
        );
        actual_sender_vk
            .verify(&payload, &self.signature)
            .map_err(|e| format!("invalid inbox-message signature: {e}"))
    }
}

/// Sign a message authored by a regular member of the room. The
/// caller supplies the [`MembershipProof`] (sender's
/// [`AuthorizedMember`] plus invitation chain).
pub fn sign_inbox_message_member(
    sender_sk: &ed25519_dalek::SigningKey,
    recipient_vk: &VerifyingKey,
    room_owner_vk: &VerifyingKey,
    timestamp: u64,
    ciphertext: Vec<u8>,
    member_proof: MembershipProof,
) -> InboxMessage {
    use ed25519_dalek::Signer;
    let sender = MemberId::from(&sender_sk.verifying_key());
    let proof_field = Some(member_proof);
    let proof_hash = compute_proof_hash(&proof_field);
    let payload = build_signed_payload_bytes(
        sender,
        recipient_vk,
        room_owner_vk,
        &proof_hash,
        timestamp,
        &ciphertext,
    );
    let signature = sender_sk.sign(&payload);
    InboxMessage {
        sender,
        timestamp,
        ciphertext,
        signature,
        member_proof: proof_field,
    }
}

/// Sign a message authored by the room owner (no membership proof).
pub fn sign_inbox_message_owner(
    owner_sk: &ed25519_dalek::SigningKey,
    recipient_vk: &VerifyingKey,
    timestamp: u64,
    ciphertext: Vec<u8>,
) -> InboxMessage {
    use ed25519_dalek::Signer;
    let owner_vk = owner_sk.verifying_key();
    let sender = MemberId::from(&owner_vk);
    let proof_field: Option<MembershipProof> = None;
    let proof_hash = compute_proof_hash(&proof_field);
    let payload = build_signed_payload_bytes(
        sender,
        recipient_vk,
        &owner_vk,
        &proof_hash,
        timestamp,
        &ciphertext,
    );
    let signature = owner_sk.sign(&payload);
    InboxMessage {
        sender,
        timestamp,
        ciphertext,
        signature,
        member_proof: proof_field,
    }
}

/// Helper: produce a recipient-signed [`AuthorizedRecipientState`].
pub fn sign_recipient_state(
    recipient_sk: &ed25519_dalek::SigningKey,
    state: RecipientState,
) -> AuthorizedRecipientState {
    use ed25519_dalek::Signer;
    let recipient_vk = recipient_sk.verifying_key();
    let payload = build_recipient_state_signed_bytes(&recipient_vk, &state);
    let signature = recipient_sk.sign(&payload);
    AuthorizedRecipientState { state, signature }
}

/// Truncated tombstone hash for a message's signature.
pub fn purge_id_for_signature(sig: &Signature) -> u32 {
    let h: FastHash = fast_hash(&sig.to_bytes());
    h.0 as u32
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Compare two messages for the canonical sort order: by
/// `(timestamp, sender, ct_len, ciphertext, signature)`. The
/// signature already disambiguates, but adding ciphertext (and
/// ct_len in front of it) makes the comparison collision-resistant
/// against same-(timestamp, sender, signature) triples that carry
/// different ciphertexts (which should not arise in practice, but is
/// cheap to defend against).
///
/// This is the inline-comparator equivalent of an earlier
/// `Vec<u8>` sort key. Avoids per-comparison allocations: with 1000
/// messages × 32 KB ciphertexts the previous approach allocated
/// ~MB of temporary buffers per sort.
fn compare_messages(a: &InboxMessage, b: &InboxMessage) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // 1. timestamp (big-endian numeric ordering == raw u64 ordering)
    match a.timestamp.cmp(&b.timestamp) {
        Ordering::Equal => {}
        non_eq => return non_eq,
    }
    // 2. sender — compare big-endian bytes of the underlying i64 to
    //    match the previous sort_key's behaviour exactly.
    match a
        .sender
        .0
         .0
        .to_be_bytes()
        .cmp(&b.sender.0 .0.to_be_bytes())
    {
        Ordering::Equal => {}
        non_eq => return non_eq,
    }
    // 3. ciphertext length (big-endian u32; identical to natural u32
    //    ordering for non-negative lengths).
    match (a.ciphertext.len() as u32).cmp(&(b.ciphertext.len() as u32)) {
        Ordering::Equal => {}
        non_eq => return non_eq,
    }
    // 4. ciphertext bytes lexicographic.
    match a.ciphertext.cmp(&b.ciphertext) {
        Ordering::Equal => {}
        non_eq => return non_eq,
    }
    // 5. signature bytes lexicographic.
    a.signature.to_bytes().cmp(&b.signature.to_bytes())
}

/// Unique deduplication key: the message signature.
fn message_dedup_key(m: &InboxMessage) -> [u8; 64] {
    m.signature.to_bytes()
}

/// Best-effort wrapper around `freenet_stdlib::time::now()`.
///
/// The non-WASM stub leaves a `MaybeUninit` initialised, which is
/// undefined behaviour to read. Inside contract WASM the host always
/// returns a real value, so this is safe in production. For native
/// (test) builds we read the system wall clock with an optional
/// thread-local override.
#[cfg(target_family = "wasm")]
fn host_now_ts() -> u64 {
    freenet_stdlib::time::now().timestamp() as u64
}

#[cfg(not(target_family = "wasm"))]
fn host_now_ts() -> u64 {
    test_clock::now()
}

#[cfg(not(target_family = "wasm"))]
mod test_clock {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};

    thread_local! {
        static OVERRIDE: Cell<Option<u64>> = const { Cell::new(None) };
    }

    pub fn now() -> u64 {
        OVERRIDE.with(|o| {
            o.get().unwrap_or_else(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            })
        })
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn set_override(ts: Option<u64>) {
        OVERRIDE.with(|o| o.set(ts));
    }
}

/// Pin the contract's notion of the current time. Test-only — gated
/// behind the `test-utils` feature so downstream consumers cannot
/// accidentally affect the contract's clock at runtime.
#[cfg(all(not(target_family = "wasm"), any(test, feature = "test-utils")))]
pub fn set_clock_override_for_tests(ts: Option<u64>) {
    test_clock::set_override(ts);
}

// ---------------------------------------------------------------------------
// Single-message validation
// ---------------------------------------------------------------------------

/// Verify a single message's authorisation: signature and (for
/// member-sent messages) the membership chain. Caller is responsible
/// for cheap shape checks (size, future-skew) — those happen
/// separately in `cheap_validate_*`.
fn verify_message_authorisation(m: &InboxMessage, params: &InboxParams) -> Result<(), String> {
    let owner_member_id = MemberId::from(&params.room_owner_vk);
    if m.sender == owner_member_id {
        // Owner-sent path. `member_proof` MUST be `None` — a non-None
        // proof would be ambiguous (and a sign of attacker confusion).
        if m.member_proof.is_some() {
            return Err("owner-sent message must not carry a member_proof".to_string());
        }
        return m.verify_signature(
            &params.room_owner_vk,
            &params.recipient_vk,
            &params.room_owner_vk,
        );
    }

    // Member-sent path.
    let proof = m
        .member_proof
        .as_ref()
        .ok_or_else(|| "member-sent message must include a member_proof".to_string())?;
    let resolved_vk = chain::verify_membership_proof(proof, &params.room_owner_vk)?;

    // The sender field MUST match the proof's sender_authorized.id().
    if proof.sender_authorized.member.id() != m.sender {
        return Err(format!(
            "InboxMessage::sender ({:?}) does not match member_proof.sender_authorized.member.id() ({:?})",
            m.sender,
            proof.sender_authorized.member.id()
        ));
    }

    m.verify_signature(&resolved_vk, &params.recipient_vk, &params.room_owner_vk)
}

/// Cheap checks on a single message — bounds and future skew. Does
/// NOT verify the signature or membership proof.
fn cheap_validate_incoming_message(m: &InboxMessage, now_ts: u64) -> Result<(), String> {
    if m.ciphertext.len() > MAX_CIPHERTEXT_BYTES {
        return Err(format!(
            "ciphertext is {} bytes, exceeds MAX_CIPHERTEXT_BYTES ({})",
            m.ciphertext.len(),
            MAX_CIPHERTEXT_BYTES
        ));
    }
    if m.timestamp > now_ts.saturating_add(MAX_FUTURE_SKEW_SECS) {
        return Err(format!(
            "timestamp {} is more than {}s ahead of host clock {}",
            m.timestamp, MAX_FUTURE_SKEW_SECS, now_ts
        ));
    }
    Ok(())
}

/// Format-only checks for already-stored messages (no future-skew
/// check — that's only for incoming messages).
fn cheap_validate_stored_message(m: &InboxMessage) -> Result<(), String> {
    if m.ciphertext.len() > MAX_CIPHERTEXT_BYTES {
        return Err(format!(
            "stored ciphertext is {} bytes, exceeds MAX_CIPHERTEXT_BYTES ({})",
            m.ciphertext.len(),
            MAX_CIPHERTEXT_BYTES
        ));
    }
    Ok(())
}

/// Verify a recipient-signed envelope's signature.
fn verify_recipient_state_signature(
    recipient_vk: &VerifyingKey,
    auth: &AuthorizedRecipientState,
) -> Result<(), String> {
    let payload = build_recipient_state_signed_bytes(recipient_vk, &auth.state);
    recipient_vk
        .verify(&payload, &auth.signature)
        .map_err(|e| format!("invalid recipient-state signature: {e}"))
}

/// Layout-only checks for an [`AuthorizedRecipientState`]: bounds and
/// signature. Does NOT check version monotonicity — that requires
/// comparing against the prior state.
fn validate_recipient_state_shape(
    recipient_vk: &VerifyingKey,
    auth: &AuthorizedRecipientState,
) -> Result<(), String> {
    if auth.state.purged.len() > MAX_PURGED_TOMBSTONES {
        return Err(format!(
            "purged list has {} entries, exceeds MAX_PURGED_TOMBSTONES ({})",
            auth.state.purged.len(),
            MAX_PURGED_TOMBSTONES
        ));
    }
    verify_recipient_state_signature(recipient_vk, auth)
}

// ---------------------------------------------------------------------------
// Contract interface
// ---------------------------------------------------------------------------

pub struct Contract;

#[contract]
impl ContractInterface for Contract {
    fn validate_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, ContractError> {
        let bytes = state.as_ref();
        if bytes.is_empty() {
            return Ok(ValidateResult::Valid);
        }
        let inbox =
            from_reader::<Inbox, &[u8]>(bytes).map_err(|e| ContractError::Deser(e.to_string()))?;
        let params = from_reader::<InboxParams, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        // Top-level bounds.
        if inbox.messages.len() > MAX_INBOX_MESSAGES {
            return Err(ContractError::InvalidUpdateWithInfo {
                reason: format!(
                    "inbox holds {} messages, exceeds MAX_INBOX_MESSAGES ({})",
                    inbox.messages.len(),
                    MAX_INBOX_MESSAGES
                ),
            });
        }

        // Recipient-state envelope (shape + signature). We
        // deliberately do not check monotonicity here:
        // `validate_state` runs in a single-state context with no
        // "prior" version available. The monotonicity gate lives in
        // `update_state`.
        if let Some(auth) = &inbox.recipient_state {
            validate_recipient_state_shape(&params.recipient_vk, auth)
                .map_err(|reason| ContractError::InvalidUpdateWithInfo { reason })?;
        }

        // Cheap stored-message checks (no future-skew on stored
        // msgs).
        for m in &inbox.messages {
            cheap_validate_stored_message(m)
                .map_err(|reason| ContractError::InvalidUpdateWithInfo { reason })?;
        }

        // Membership + signature check, per message.
        for m in &inbox.messages {
            verify_message_authorisation(m, &params)
                .map_err(|reason| ContractError::InvalidUpdateWithInfo { reason })?;
        }

        Ok(ValidateResult::Valid)
    }

    fn update_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<UpdateModification<'static>, ContractError> {
        let params = from_reader::<InboxParams, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        let mut inbox: Inbox = if state.as_ref().is_empty() {
            Inbox::default()
        } else {
            from_reader::<Inbox, &[u8]>(state.as_ref())
                .map_err(|e| ContractError::Deser(e.to_string()))?
        };

        let now_ts = host_now_ts();

        for update in data {
            match update {
                UpdateData::State(new_state) => {
                    let new = from_reader::<Inbox, &[u8]>(new_state.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    apply_full_state(&mut inbox, new, now_ts, &params)?;
                }
                UpdateData::Delta(d) => {
                    if d.as_ref().is_empty() {
                        continue;
                    }
                    let delta = from_reader::<InboxDelta, &[u8]>(d.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    apply_delta(&mut inbox, delta, now_ts, &params)?;
                }
                UpdateData::StateAndDelta {
                    state: new_state,
                    delta,
                } => {
                    let new = from_reader::<Inbox, &[u8]>(new_state.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    apply_full_state(&mut inbox, new, now_ts, &params)?;
                    if !delta.as_ref().is_empty() {
                        let parsed = from_reader::<InboxDelta, &[u8]>(delta.as_ref())
                            .map_err(|e| ContractError::Deser(e.to_string()))?;
                        apply_delta(&mut inbox, parsed, now_ts, &params)?;
                    }
                }
                _ => {
                    return Err(ContractError::InvalidUpdate);
                }
            }
        }

        let mut out = Vec::new();
        into_writer(&inbox, &mut out).map_err(|e| ContractError::Deser(e.to_string()))?;
        Ok(UpdateModification::valid(out.into()))
    }

    fn summarize_state(
        _parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, ContractError> {
        let bytes = state.as_ref();
        let summary = if bytes.is_empty() {
            InboxSummary::default()
        } else {
            let inbox = from_reader::<Inbox, &[u8]>(bytes)
                .map_err(|e| ContractError::Deser(e.to_string()))?;
            InboxSummary {
                message_signatures: inbox
                    .messages
                    .iter()
                    .map(|m| m.signature.to_bytes().to_vec())
                    .collect(),
                recipient_state_version: inbox
                    .recipient_state
                    .as_ref()
                    .map(|a| a.state.version)
                    .unwrap_or(0),
            }
        };
        let mut out = Vec::new();
        into_writer(&summary, &mut out).map_err(|e| ContractError::Deser(e.to_string()))?;
        Ok(StateSummary::from(out))
    }

    fn get_state_delta(
        _parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, ContractError> {
        let inbox = if state.as_ref().is_empty() {
            Inbox::default()
        } else {
            from_reader::<Inbox, &[u8]>(state.as_ref())
                .map_err(|e| ContractError::Deser(e.to_string()))?
        };
        let summary: InboxSummary = if summary.as_ref().is_empty() {
            InboxSummary::default()
        } else {
            from_reader::<InboxSummary, &[u8]>(summary.as_ref())
                .map_err(|e| ContractError::Deser(e.to_string()))?
        };

        let known: HashSet<[u8; 64]> = summary
            .message_signatures
            .into_iter()
            .filter_map(|s| {
                let arr: Result<[u8; 64], _> = s.try_into();
                arr.ok()
            })
            .collect();
        let missing: Vec<InboxMessage> = inbox
            .messages
            .iter()
            .filter(|m| !known.contains(&m.signature.to_bytes()))
            .cloned()
            .collect();

        // Only emit a delta if there's something new to send. We
        // currently only ferry messages — recipient_state replacement
        // is a separate (signed) flow that the recipient initiates
        // explicitly via `UpdateRecipientState`.
        if missing.is_empty() {
            return Ok(StateDelta::from(Vec::new()));
        }
        let delta = InboxDelta::AppendMessages(missing);
        let mut out = Vec::new();
        into_writer(&delta, &mut out).map_err(|e| ContractError::Deser(e.to_string()))?;
        Ok(StateDelta::from(out))
    }
}

// ---------------------------------------------------------------------------
// Update helpers
// ---------------------------------------------------------------------------

/// The current tombstone set, derived from the recipient state.
fn current_tombstones(inbox: &Inbox) -> HashSet<u32> {
    inbox
        .recipient_state
        .as_ref()
        .map(|a| a.state.purged.iter().copied().collect())
        .unwrap_or_default()
}

/// Apply a `Vec<InboxMessage>` from an `AppendMessages` delta or an
/// `UpdateData::State` payload. Caps are enforced against running
/// totals (NOT post-merge) so an oversize batch is rejected
/// immediately rather than after intermediate state has been mutated.
fn apply_append(
    inbox: &mut Inbox,
    new_messages: Vec<InboxMessage>,
    now_ts: u64,
    params: &InboxParams,
) -> Result<(), ContractError> {
    let tombstones = current_tombstones(inbox);
    let mut have: HashSet<[u8; 64]> = inbox.messages.iter().map(message_dedup_key).collect();

    for m in new_messages {
        cheap_validate_incoming_message(&m, now_ts)
            .map_err(|reason| ContractError::InvalidUpdateWithInfo { reason })?;
        verify_message_authorisation(&m, params)
            .map_err(|reason| ContractError::InvalidUpdateWithInfo { reason })?;

        // Tombstone block — the recipient already explicitly purged a
        // message with this signature hash; senders cannot
        // re-introduce it by replay.
        let pid = purge_id_for_signature(&m.signature);
        if tombstones.contains(&pid) {
            return Err(ContractError::InvalidUpdateWithInfo {
                reason: "message hash matches a recipient-tombstone entry; replay rejected"
                    .to_string(),
            });
        }

        let k = message_dedup_key(&m);
        if !have.insert(k) {
            // Duplicate of an existing message — drop silently.
            // (Idempotent.)
            continue;
        }

        // Length running total.
        if inbox.messages.len() >= MAX_INBOX_MESSAGES {
            return Err(ContractError::InvalidUpdateWithInfo {
                reason: format!(
                    "appending would push inbox to {} messages, exceeds MAX_INBOX_MESSAGES ({})",
                    inbox.messages.len() + 1,
                    MAX_INBOX_MESSAGES
                ),
            });
        }

        inbox.messages.push(m);
    }
    inbox.messages.sort_by(compare_messages);
    Ok(())
}

/// Apply a delta.
fn apply_delta(
    inbox: &mut Inbox,
    delta: InboxDelta,
    now_ts: u64,
    params: &InboxParams,
) -> Result<(), ContractError> {
    match delta {
        InboxDelta::AppendMessages(msgs) => apply_append(inbox, msgs, now_ts, params),
        InboxDelta::UpdateRecipientState(auth) => apply_update_recipient_state(inbox, auth, params),
    }
}

/// Apply a recipient-state replacement. Validates shape, signature,
/// and strict version monotonicity; then drops any messages whose
/// hashes are in the new tombstone list.
fn apply_update_recipient_state(
    inbox: &mut Inbox,
    auth: AuthorizedRecipientState,
    params: &InboxParams,
) -> Result<(), ContractError> {
    validate_recipient_state_shape(&params.recipient_vk, &auth)
        .map_err(|reason| ContractError::InvalidUpdateWithInfo { reason })?;

    let prior_version = inbox
        .recipient_state
        .as_ref()
        .map(|a| a.state.version)
        .unwrap_or(0);
    if auth.state.version <= prior_version {
        return Err(ContractError::InvalidUpdateWithInfo {
            reason: format!(
                "recipient_state version {} is not greater than current {}",
                auth.state.version, prior_version
            ),
        });
    }

    let new_tombstones: HashSet<u32> = auth.state.purged.iter().copied().collect();
    inbox.recipient_state = Some(auth);
    inbox
        .messages
        .retain(|m| !new_tombstones.contains(&purge_id_for_signature(&m.signature)));
    Ok(())
}

/// Apply an `UpdateData::State` payload. Treats incoming `messages`
/// as `AppendMessages`, and incoming `recipient_state` (if any) as an
/// `UpdateRecipientState`.
///
/// Symmetric with [`apply_delta`]: a stale incoming `recipient_state`
/// (version <= the existing one's) is rejected with the same error
/// shape `apply_update_recipient_state` produces for stale deltas. A
/// malicious peer must not be able to mask an old-version replay by
/// sending it as `UpdateData::State` instead of as a delta.
fn apply_full_state(
    inbox: &mut Inbox,
    new: Inbox,
    now_ts: u64,
    params: &InboxParams,
) -> Result<(), ContractError> {
    if let Some(auth) = new.recipient_state {
        apply_update_recipient_state(inbox, auth, params)?;
    }
    apply_append(inbox, new.messages, now_ts, params)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
