//! Public-room provisional admission (freenet/river#974, Phase 1).
//!
//! A public room whose owner signed a [`PublicRoomPolicy`] lets any key post
//! after attaching a [`AuthorizedProvisionalMember`]: the key's own signature
//! over a proof of work bound to (room owner, member key, epoch). Provisional
//! members live in [`ProvisionalV1`], never in `members`, so every reader of
//! `members` (invitations, bans, deputies, DMs, secrets, `max_members`) grants
//! them nothing. Provisional ids are never put into any `members` lookup map.
//!
//! The rules (arrival here, cleanup in `post_apply_cleanup`) are the frozen
//! design in river#974. Short form:
//! - arrival only adds, dedups and sorts; an entry that fails the CURRENT
//!   policy is skipped (another peer may hold a configuration under which it
//!   passes), a forged signature is an `Err`;
//! - cleanup re-decides the whole provisional set from converged state
//!   ([`ChatRoomStateV1::provisional_step`] and the member loop's window scan);
//! - a `members` or ban-evidence record always beats an entry with the same
//!   `MemberId` and a different key, and an entry never removes a record.

use crate::room_state::configuration::Configuration;
use crate::room_state::member::{AuthorizedMember, MemberId};
use crate::room_state::privacy::PrivacyMode;
use crate::room_state::ChatRoomParametersV1;
use crate::signed_time::SignedTime;
use crate::ChatRoomStateV1;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Signing domain of a provisional member's self-signature.
pub const ENTRY_SIGNING_TAG: &[u8] = b"river-provisional-member/v1";
/// Signing domain of a provisional ban. Differs from any `UserBan` bytes, so
/// neither can be replayed as the other.
pub const BAN_SIGNING_TAG: &[u8] = b"river-provisional-ban/v1";
/// Domain tag of the proof-of-work hash (27 bytes).
pub const POW_DOMAIN_TAG: &[u8; 27] = b"river-room-admission-pow/v1";

/// Largest `max_provisional` a policy may set. Set by the cost gate, not by
/// taste: the host re-verifies every held entry's signature, its `member_info`
/// and its message, and every provisional ban, in `validate_state` after every
/// chat message, plus every ban again in `update_state`'s cleanup. Measured
/// under wasmtime (about 0.4 ms a signature) that is about 2.5 ms per entry and
/// ban pair: 100 and 100 added over 300 ms to a pair that must stay under
/// 100 ms (design section 5: "if the pair exceeds 100 ms, lower the ceilings"),
/// and 20 and 20 still measured 100.3 ms on a default-sized room (window 100,
/// 26 members). At 16 and 16 a one-message delta measured about 86 ms, and
/// the worst-case delta (every entry and ban carried) about 130 ms, over the
/// 100 ms gate, which is set for one-message deltas; the worst case needs a
/// delta that fills the per-delta floor and is paid once per such delta.
/// Raising it needs a measurement, not an opinion: the benchmark
/// is `common/benches/provisional_pair.rs` (native) and the same fixtures run
/// under wasmtime in the PR.
pub const MAX_PROVISIONAL_CEILING: u32 = 16;
/// Largest `max_provisional_messages` a policy may set.
pub const MAX_PROVISIONAL_MESSAGES_CEILING: u32 = 500;
/// Largest `max_provisional_bans` a policy may set (see
/// [`MAX_PROVISIONAL_CEILING`] for why it is this low).
pub const MAX_PROVISIONAL_BANS_CEILING: u32 = 16;
/// Largest `difficulty_bits` a policy may set.
pub const MAX_DIFFICULTY_BITS: u8 = 32;
/// The least number of entries or provisional bans one delta may carry: a work
/// bound, never below the caps (the ceilings are 16).
pub const PROVISIONAL_DELTA_FLOOR: usize = 16;

// --- policy (owner-signed, inside `Configuration`) ---------------------------

/// Open admission for `PrivacyMode::Public` rooms (#974). Absent from a
/// configuration (`None`) means invite-only.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PublicRoomPolicy {
    /// Atlas discovery opt-in; no effect on admission.
    pub listed: bool,
    /// Provisional authors held at once, `1..=16` ([`MAX_PROVISIONAL_CEILING`]).
    ///
    /// When the pool is full a new author is not refused for being one too
    /// many: the pool is the authors of the newest provisional messages in the
    /// window (ties broken by message id), so the author whose newest message
    /// is oldest falls out, with its entry and messages, and may come back by
    /// sending a newer message. A newcomer whose message is older than every
    /// pooled author's is the one dropped (`RoomFull`).
    pub max_provisional: u32,
    /// Window slots for provisional messages, `1..=500`. The effective quota is
    /// `min(this, max_recent_messages / 2)` ([`Self::quota`]).
    pub max_provisional_messages: u32,
    /// Provisional ban list size, `0..=16` ([`MAX_PROVISIONAL_BANS_CEILING`]).
    pub max_provisional_bans: u32,
    /// `None` admits nobody (a listed, invite-only room).
    pub pow: Option<PowPolicy>,
}

/// Proof-of-work requirement.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PowPolicy {
    /// Leading zero bits the hash must have, `0..=32`.
    pub difficulty_bits: u8,
    /// Bound into the hash; a new random value invalidates every proof.
    pub epoch: u32,
}

impl PublicRoomPolicy {
    /// The absolute range checks (never against another field: a failing
    /// configuration ranks below every valid one, so a check that depended on
    /// another field would silently invalidate an owner's later change to it).
    pub fn check_values(&self) -> Result<(), String> {
        if !(1..=MAX_PROVISIONAL_CEILING).contains(&self.max_provisional) {
            return Err(format!(
                "max_provisional must be 1..={MAX_PROVISIONAL_CEILING}"
            ));
        }
        if !(1..=MAX_PROVISIONAL_MESSAGES_CEILING).contains(&self.max_provisional_messages) {
            return Err(format!(
                "max_provisional_messages must be 1..={MAX_PROVISIONAL_MESSAGES_CEILING}"
            ));
        }
        if self.max_provisional_bans > MAX_PROVISIONAL_BANS_CEILING {
            return Err(format!(
                "max_provisional_bans must be at most {MAX_PROVISIONAL_BANS_CEILING}"
            ));
        }
        if let Some(pow) = &self.pow {
            if pow.difficulty_bits > MAX_DIFFICULTY_BITS {
                return Err(format!(
                    "difficulty_bits must be at most {MAX_DIFFICULTY_BITS}"
                ));
            }
        }
        Ok(())
    }

    /// Window slots provisional messages may fill, clamped at use:
    /// `min(max_provisional_messages, max_recent_messages / 2)`.
    pub fn quota(&self, max_recent_messages: usize) -> usize {
        (self
            .max_provisional_messages
            .min(MAX_PROVISIONAL_MESSAGES_CEILING) as usize)
            .min(max_recent_messages / 2)
    }

    /// Provisional authors the room holds at once, clamped to the ceiling at
    /// use: a configuration that fails the value checks can be the held one,
    /// and no bound may follow a value that large.
    pub fn pool_cap(&self) -> usize {
        self.max_provisional.min(MAX_PROVISIONAL_CEILING) as usize
    }

    /// Provisional bans the room holds, clamped to the ceiling at use.
    pub fn ban_cap(&self) -> usize {
        self.max_provisional_bans.min(MAX_PROVISIONAL_BANS_CEILING) as usize
    }
}

/// The policy in force: only a `Public` room with a policy admits anybody. A
/// policy on a `Private` configuration is valid and inert.
pub fn active_policy(config: &Configuration) -> Option<&PublicRoomPolicy> {
    match (&config.privacy_mode, &config.public_admission) {
        (PrivacyMode::Public, Some(policy)) => Some(policy),
        _ => None,
    }
}

/// Most provisional entries a state may hold under `config` (zero with no
/// policy or a private room).
pub fn entries_cap(config: &Configuration) -> usize {
    active_policy(config).map_or(0, PublicRoomPolicy::pool_cap)
}

/// Most provisional bans a state may hold under `config`.
pub fn bans_cap(config: &Configuration) -> usize {
    active_policy(config).map_or(0, PublicRoomPolicy::ban_cap)
}

/// Most NEW entries one delta may carry in a room with `config`:
/// `max(max_provisional, 16)`.
pub fn entries_work_bound(config: &Configuration) -> usize {
    config
        .public_admission
        .as_ref()
        .map_or(0, PublicRoomPolicy::pool_cap)
        .max(PROVISIONAL_DELTA_FLOOR)
}

/// Most NEW provisional bans one delta may carry: `max(max_provisional_bans, 16)`.
pub fn bans_work_bound(config: &Configuration) -> usize {
    config
        .public_admission
        .as_ref()
        .map_or(0, PublicRoomPolicy::ban_cap)
        .max(PROVISIONAL_DELTA_FLOOR)
}

// --- records -----------------------------------------------------------------

/// What a provisional member is: its key and the evidence it was admitted on.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ProvisionalMember {
    pub member_vk: VerifyingKey,
    pub evidence: AdmissionEvidence,
}

/// Externally tagged. Phase 2 adds a `GhostKey` variant; `Pow` entries keep
/// their bytes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AdmissionEvidence {
    Pow { nonce: u64 },
}

/// A provisional member with its own signature over
/// `ENTRY_SIGNING_TAG || owner_vk || ciborium(provisional)`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedProvisionalMember {
    pub provisional: ProvisionalMember,
    pub signature: Signature,
}

/// A ban of a provisional member, by the owner or an owner deputy.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ProvisionalBan {
    pub banned_by: MemberId,
    /// Display only; never used to order anything.
    pub banned_at: SignedTime,
    pub target: MemberId,
}

/// A provisional ban with its issuer's signature over
/// `BAN_SIGNING_TAG || owner_vk || ciborium(ban)`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedProvisionalBan {
    pub ban: ProvisionalBan,
    pub signature: Signature,
}

macro_rules! digest16 {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub [u8; 16]);

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_bytes(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let bytes = <Vec<u8>>::deserialize(deserializer)?;
                let arr: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
                    serde::de::Error::custom(format!(
                        "expected a 16-byte digest, got {} bytes",
                        bytes.len()
                    ))
                })?;
                Ok($name(arr))
            }
        }
    };
}

digest16! {
    /// The first 16 bytes of BLAKE3 over a whole entry's CBOR, serialized as a
    /// CBOR byte string. Wire format (pinned by a golden vector): the summary
    /// compares it. 128 bits because a party can grind both records compared.
    EntryDigest
}
digest16! {
    /// [`EntryDigest`] for a provisional ban.
    BanDigest
}

fn digest_of<T: Serialize>(value: &T) -> [u8; 16] {
    let full = crate::util::record_digest(value);
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    out
}

/// The digest of a whole entry.
pub fn entry_digest(entry: &AuthorizedProvisionalMember) -> EntryDigest {
    EntryDigest(digest_of(entry))
}

/// The digest of a whole provisional ban.
pub fn ban_digest(ban: &AuthorizedProvisionalBan) -> BanDigest {
    BanDigest(digest_of(ban))
}

fn cbor<T: Serialize>(value: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(value, &mut out).expect("Serialization should not fail");
    out
}

/// The bytes a provisional member signs.
pub fn provisional_signing_bytes(owner_vk: &VerifyingKey, member: &ProvisionalMember) -> Vec<u8> {
    let mut out = ENTRY_SIGNING_TAG.to_vec();
    out.extend_from_slice(owner_vk.as_bytes());
    out.extend_from_slice(&cbor(member));
    out
}

/// The bytes a provisional ban's issuer signs.
pub fn provisional_ban_signing_bytes(owner_vk: &VerifyingKey, ban: &ProvisionalBan) -> Vec<u8> {
    let mut out = BAN_SIGNING_TAG.to_vec();
    out.extend_from_slice(owner_vk.as_bytes());
    out.extend_from_slice(&cbor(ban));
    out
}

impl AuthorizedProvisionalMember {
    /// Sign `member` with the member's own key.
    pub fn new(
        owner_vk: &VerifyingKey,
        member: ProvisionalMember,
        signing_key: &SigningKey,
    ) -> Self {
        let signature = signing_key.sign(&provisional_signing_bytes(owner_vk, &member));
        Self {
            provisional: member,
            signature,
        }
    }

    /// An entry whose signature was made elsewhere (a key vault).
    pub fn with_signature(member: ProvisionalMember, signature: Signature) -> Self {
        Self {
            provisional: member,
            signature,
        }
    }

    pub fn member_id(&self) -> MemberId {
        MemberId::from(&self.provisional.member_vk)
    }

    /// `entry_sig_ok`: the member's self-signature verifies. Memoized for the
    /// whole `merge_from` / `validate_whole_state` call.
    pub fn verify_signature(&self, owner_vk: &VerifyingKey) -> Result<(), String> {
        let key = (
            crate::util::record_digest(self),
            owner_vk.to_bytes(),
            self.provisional.member_vk.to_bytes(),
            0u8,
        );
        if let Some(ok) = crate::room_state::ban::signature_memo::lookup_provisional(&key) {
            return ok_or_invalid(ok, "provisional member");
        }
        // `verify_strict`: also refuses a small-order key or `R`, so no
        // signature verifies without the key's secret.
        let ok = crate::room_state::sigcheck::verify(
            crate::room_state::sigcheck::Kind::Strict,
            &self.provisional.member_vk,
            &provisional_signing_bytes(owner_vk, &self.provisional),
            &self.signature,
        )
        .is_ok();
        crate::room_state::ban::signature_memo::record_provisional(key, ok);
        ok_or_invalid(ok, "provisional member")
    }

    /// `policy_ok`: the entry admits under `config`'s policy.
    pub fn check_policy(
        &self,
        config: &Configuration,
        owner_vk: &VerifyingKey,
    ) -> Result<(), AdmissionError> {
        if config.privacy_mode != PrivacyMode::Public {
            return Err(AdmissionError::NotPublic);
        }
        let policy = config
            .public_admission
            .as_ref()
            .ok_or(AdmissionError::NoPolicy)?;
        let pow = policy.pow.as_ref().ok_or(AdmissionError::PowDisabled)?;
        if self.provisional.member_vk == *owner_vk {
            return Err(AdmissionError::OwnerKey);
        }
        // A small-order key: a signature under it can be made without any
        // secret key, so it proves nothing about who posted.
        if self.provisional.member_vk.is_weak() {
            return Err(AdmissionError::WeakKey);
        }
        match &self.provisional.evidence {
            AdmissionEvidence::Pow { nonce } => {
                let achieved = leading_zero_bits(&pow_hash(
                    owner_vk,
                    &self.provisional.member_vk,
                    pow.epoch,
                    *nonce,
                ));
                if achieved < pow.difficulty_bits as u32 {
                    return Err(AdmissionError::InsufficientWork {
                        required: pow.difficulty_bits,
                        achieved: achieved.min(255) as u8,
                    });
                }
            }
        }
        Ok(())
    }
}

fn ok_or_invalid(ok: bool, what: &str) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(format!("Invalid {what} signature"))
    }
}

impl AuthorizedProvisionalBan {
    /// Sign `ban` with the issuer's key.
    pub fn new(owner_vk: &VerifyingKey, ban: ProvisionalBan, signing_key: &SigningKey) -> Self {
        let signature = signing_key.sign(&provisional_ban_signing_bytes(owner_vk, &ban));
        Self { ban, signature }
    }

    /// The issuer's signature verifies under `issuer_vk`. Memoized.
    pub fn verify_signature(
        &self,
        owner_vk: &VerifyingKey,
        issuer_vk: &VerifyingKey,
    ) -> Result<(), String> {
        let key = (
            crate::util::record_digest(self),
            owner_vk.to_bytes(),
            issuer_vk.to_bytes(),
            1u8,
        );
        if let Some(ok) = crate::room_state::ban::signature_memo::lookup_provisional(&key) {
            return ok_or_invalid(ok, "provisional ban");
        }
        // `verify_strict`, as for entry signatures: also refuses a small-order
        // key or `R`.
        let ok = crate::room_state::sigcheck::verify(
            crate::room_state::sigcheck::Kind::Strict,
            issuer_vk,
            &provisional_ban_signing_bytes(owner_vk, &self.ban),
            &self.signature,
        )
        .is_ok();
        crate::room_state::ban::signature_memo::record_provisional(key, ok);
        ok_or_invalid(ok, "provisional ban")
    }
}

/// Why an entry does not admit under a configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionError {
    /// The room is not `Public`.
    NotPublic,
    /// The configuration has no `public_admission`.
    NoPolicy,
    /// The policy has no proof-of-work requirement: nobody is admitted.
    PowDisabled,
    /// The entry's key is the room owner's.
    OwnerKey,
    /// The entry's key has small order, so it signs nothing.
    WeakKey,
    /// The proof has fewer leading zero bits than the policy requires.
    InsufficientWork { required: u8, achieved: u8 },
    /// The member's self-signature does not verify.
    BadSignature,
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPublic => write!(f, "the room is not public"),
            Self::NoPolicy => write!(f, "the room has no open-admission policy"),
            Self::PowDisabled => write!(f, "the room does not admit by proof of work"),
            Self::OwnerKey => write!(f, "the key is the room owner's"),
            Self::WeakKey => write!(f, "the key is a weak (small-order) key"),
            Self::InsufficientWork { required, achieved } => {
                write!(f, "proof of work has {achieved} bits, {required} required")
            }
            Self::BadSignature => write!(f, "the self-signature does not verify"),
        }
    }
}

// --- proof of work -----------------------------------------------------------

/// `BLAKE3(POW_DOMAIN_TAG || owner_vk || member_vk || epoch (u32 BE) || nonce (u64 BE))`.
pub fn pow_hash(
    owner_vk: &VerifyingKey,
    member_vk: &VerifyingKey,
    epoch: u32,
    nonce: u64,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(POW_DOMAIN_TAG);
    hasher.update(owner_vk.as_bytes());
    hasher.update(member_vk.as_bytes());
    hasher.update(&epoch.to_be_bytes());
    hasher.update(&nonce.to_be_bytes());
    *hasher.finalize().as_bytes()
}

/// Leading zero bits, counted from byte 0, most significant bit first.
pub fn leading_zero_bits(hash: &[u8; 32]) -> u32 {
    let mut bits = 0;
    for byte in hash {
        if *byte == 0 {
            bits += 8;
        } else {
            bits += byte.leading_zeros();
            break;
        }
    }
    bits
}

// --- the state field ---------------------------------------------------------

/// Provisional members and provisional bans. Unsigned container: skipped when
/// empty, so a room that never used open admission keeps its exact bytes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ProvisionalV1 {
    /// Sorted by `(MemberId, EntryDigest)`.
    pub entries: Vec<AuthorizedProvisionalMember>,
    /// Sorted by `BanDigest`.
    pub bans: Vec<AuthorizedProvisionalBan>,
}

/// The provisional field's summary: the digests held.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct ProvisionalSummary {
    pub entries: BTreeSet<(MemberId, EntryDigest)>,
    pub bans: BTreeSet<BanDigest>,
}

/// The records a receiver's summary lacks.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ProvisionalDelta {
    pub entries: Vec<AuthorizedProvisionalMember>,
    pub bans: Vec<AuthorizedProvisionalBan>,
}

impl ProvisionalV1 {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.bans.is_empty()
    }

    /// The key each entry id resolves to: its entries' common key, or `None`
    /// when the held entries for the id disagree. Read by the `verify` of
    /// `member_info` and messages, on a whole state; an apply reads
    /// [`Self::admitted_keys`] instead.
    pub fn key_map(&self) -> HashMap<MemberId, Option<VerifyingKey>> {
        let mut map: HashMap<MemberId, Option<VerifyingKey>> = HashMap::new();
        for entry in &self.entries {
            let vk = entry.provisional.member_vk;
            map.entry(entry.member_id())
                .and_modify(|held| {
                    if *held != Some(vk) {
                        *held = None;
                    }
                })
                .or_insert(Some(vk));
        }
        map
    }

    /// Every key a held entry that passes the CURRENT policy carries, per
    /// id: the keys a provisional author's message or `member_info` may be
    /// signed with at this point of an apply (river#974, 2.6).
    ///
    /// All of them, not one resolved key: a record that verifies under any
    /// of them is accepted on arrival, and cleanup (P3, C1, C2, C3) decides
    /// which survive from the merged records. Resolving to one key here
    /// would make "skipped on arrival" read records cleanup may drop (an
    /// entry failing the policy, an evidence record deeper than the cap)
    /// while "dropped where held" does not, so the first exchange of a merge
    /// would depend on which side held the record. Entries failing the
    /// current policy are left out because cleanup drops them (P3) together
    /// with everything their key signed.
    pub fn admitted_keys(
        &self,
        config: &Configuration,
        owner_vk: &VerifyingKey,
    ) -> HashMap<MemberId, Vec<VerifyingKey>> {
        let mut map: HashMap<MemberId, Vec<VerifyingKey>> = HashMap::new();
        for entry in &self.entries {
            if entry.check_policy(config, owner_vk).is_err() {
                continue;
            }
            let keys = map.entry(entry.member_id()).or_default();
            if !keys.contains(&entry.provisional.member_vk) {
                keys.push(entry.provisional.member_vk);
            }
        }
        map
    }

    pub(crate) fn sort_canonical(&mut self) {
        self.entries
            .sort_by_cached_key(|e| (e.member_id(), entry_digest(e)));
        self.bans.sort_by_cached_key(ban_digest);
    }
}

impl ComposableState for ProvisionalV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = ProvisionalSummary;
    type Delta = ProvisionalDelta;
    type Parameters = ChatRoomParametersV1;

    /// Every held entry's self-signature, every held ban signature whose
    /// issuer is known, and the counts against the state's own caps. This is
    /// the cost `validate_state` pays after every changing update, so the
    /// ceilings are 16 each ([`MAX_PROVISIONAL_CEILING`]).
    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        let config = &parent_state.configuration.configuration;
        if self.entries.len() > entries_cap(config) {
            return Err(format!(
                "Number of provisional entries ({}) exceeds the maximum allowed ({})",
                self.entries.len(),
                entries_cap(config)
            ));
        }
        if self.bans.len() > bans_cap(config) {
            return Err(format!(
                "Number of provisional bans ({}) exceeds the maximum allowed ({})",
                self.bans.len(),
                bans_cap(config)
            ));
        }
        for entry in &self.entries {
            entry.verify_signature(&parameters.owner)?;
        }
        let owner_id = parameters.owner_id();
        let known = parent_state
            .ban_evidence
            .lookup(&parent_state.members, owner_id);
        for ban in &self.bans {
            if ban.ban.banned_by == owner_id {
                ban.verify_signature(&parameters.owner, &parameters.owner)?;
            } else if let Some(issuer) = known.get(&ban.ban.banned_by) {
                ban.verify_signature(&parameters.owner, &issuer.member.member_vk)?;
            }
            // An issuer known nowhere: no key to check against; cleanup sweeps it.
        }
        Ok(())
    }

    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        ProvisionalSummary {
            entries: self
                .entries
                .iter()
                .map(|e| (e.member_id(), entry_digest(e)))
                .collect(),
            bans: self.bans.iter().map(ban_digest).collect(),
        }
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        let entries: Vec<_> = self
            .entries
            .iter()
            .filter(|e| {
                !old_state_summary
                    .entries
                    .contains(&(e.member_id(), entry_digest(e)))
            })
            .cloned()
            .collect();
        let bans: Vec<_> = self
            .bans
            .iter()
            .filter(|b| !old_state_summary.bans.contains(&ban_digest(b)))
            .cloned()
            .collect();
        if entries.is_empty() && bans.is_empty() {
            None
        } else {
            Some(ProvisionalDelta { entries, bans })
        }
    }

    /// Arrival: only adds, dedups and sorts. Removal and every cap is decided
    /// in `post_apply_cleanup` from the merged records.
    ///
    /// - An entry not already held whose self-signature fails is an `Err`: it
    ///   carries the only key it can be checked against, so that is a forgery.
    /// - An entry that fails the CURRENT policy is skipped, not an error:
    ///   another peer may hold a configuration under which it passes.
    /// - Collisions (C1, C2) and sponsorship (P5) are decided only in cleanup,
    ///   so arrival never depends on which records happen to be held now.
    /// - A ban issued by the owner is checked against the owner key (`Err` on
    ///   failure); one by an issuer with a `members` or evidence record is
    ///   checked against that record's key and SKIPPED on failure (another
    ///   peer may hold a different record for the id); one by an issuer known
    ///   nowhere is kept and swept by cleanup. Authority is judged only in
    ///   cleanup, after `member_info` has applied.
    fn apply_delta(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        let config = &parent_state.configuration.configuration;
        if let Some(delta) = delta {
            let lifted = crate::room_state::work_bounds::lifted();
            let entry_bound = entries_work_bound(config);
            let ban_bound = bans_work_bound(config);
            // A raw length past held plus the bound is refused before any
            // record is hashed.
            if !lifted && delta.entries.len() > self.entries.len().saturating_add(entry_bound) {
                return Err(format!(
                    "Provisional entry delta of {} exceeds {entry_bound}",
                    delta.entries.len()
                ));
            }
            if !lifted && delta.bans.len() > self.bans.len().saturating_add(ban_bound) {
                return Err(format!(
                    "Provisional ban delta of {} exceeds {ban_bound}",
                    delta.bans.len()
                ));
            }
            // A room with no active policy (no policy, or not `Public`)
            // holds no entry and no provisional ban after cleanup (P0), so
            // nothing in the delta is hashed or checked: a room that never
            // enabled open admission pays nothing for this field.
            if active_policy(config).is_some() {
                self.apply_new_records(parent_state, parameters, delta, lifted)?;
            }
        }

        // Held twice (a state `verify` accepts but no apply produces): kept once.
        let mut seen = HashSet::new();
        self.entries
            .retain(|e| seen.insert((e.member_id(), entry_digest(e))));
        let mut seen = HashSet::new();
        self.bans.retain(|b| seen.insert(ban_digest(b)));
        self.sort_canonical();
        Ok(())
    }
}

impl ProvisionalV1 {
    /// The arrival half of [`ComposableState::apply_delta`] for a room with
    /// an active policy: the records the delta carries that are not held.
    fn apply_new_records(
        &mut self,
        parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
        delta: &ProvisionalDelta,
        lifted: bool,
    ) -> Result<(), String> {
        let config = &parent_state.configuration.configuration;
        let entry_bound = entries_work_bound(config);
        let ban_bound = bans_work_bound(config);
        {
            let held: HashSet<(MemberId, EntryDigest)> = self
                .entries
                .iter()
                .map(|e| (e.member_id(), entry_digest(e)))
                .collect();
            let mut in_delta = HashSet::new();
            let new_entries: Vec<&AuthorizedProvisionalMember> = delta
                .entries
                .iter()
                .filter(|e| {
                    let key = (e.member_id(), entry_digest(e));
                    !held.contains(&key) && in_delta.insert(key)
                })
                .collect();
            if !lifted && new_entries.len() > entry_bound {
                return Err(format!(
                    "Provisional entry delta of {} new entries exceeds {entry_bound}",
                    new_entries.len()
                ));
            }
            // The policy check (one BLAKE3 call) runs first, so an entry
            // that cannot be admitted costs no signature check.
            for entry in new_entries {
                if entry.check_policy(config, &parameters.owner).is_ok() {
                    entry.verify_signature(&parameters.owner)?;
                    self.entries.push(entry.clone());
                }
            }

            let held_bans: HashSet<BanDigest> = self.bans.iter().map(ban_digest).collect();
            let mut in_delta = HashSet::new();
            let new_bans: Vec<&AuthorizedProvisionalBan> = delta
                .bans
                .iter()
                .filter(|b| {
                    let d = ban_digest(b);
                    !held_bans.contains(&d) && in_delta.insert(d)
                })
                .collect();
            if !lifted && new_bans.len() > ban_bound {
                return Err(format!(
                    "Provisional ban delta of {} new bans exceeds {ban_bound}",
                    new_bans.len()
                ));
            }
            let owner_id = parameters.owner_id();
            let known = parent_state
                .ban_evidence
                .lookup(&parent_state.members, owner_id);
            for ban in new_bans {
                if ban.ban.banned_by == owner_id {
                    ban.verify_signature(&parameters.owner, &parameters.owner)?;
                    self.bans.push(ban.clone());
                } else if let Some(issuer) = known.get(&ban.ban.banned_by) {
                    if ban
                        .verify_signature(&parameters.owner, &issuer.member.member_vk)
                        .is_ok()
                    {
                        self.bans.push(ban.clone());
                    }
                } else {
                    self.bans.push(ban.clone());
                }
            }
        }
        Ok(())
    }
}

// --- cleanup -----------------------------------------------------------------

/// What `provisional_step` fixed for the member loop.
#[derive(Default)]
pub(crate) struct ProvisionalPlan {
    /// Ids of the provisional authors that may post: the entries surviving
    /// P0 to P5 (none has a record in the C1 set with another key; none is
    /// sponsored).
    pub ids: HashSet<MemberId>,
    /// `min(max_provisional_messages, max_recent_messages / 2)`.
    pub quota: usize,
    /// `max_provisional`.
    pub pool_cap: usize,
    /// Issuers of the kept provisional bans: required ids in the member loop.
    pub ban_issuers: HashSet<MemberId>,
    /// The anchor of the P6 time bound (see [`PROVISIONAL_TIME_SLACK`]), or
    /// `None` when no message exists to anchor it. Fixed once per pass here,
    /// so the member loop and step 5b rank by the same value.
    pub anchor: Option<SignedTime>,
}

/// A message's position in the P6 window scan; larger is newer.
pub(crate) type ScanKey = (SignedTime, bool, crate::room_state::message::MessageId);

impl ProvisionalPlan {
    /// The key the P6 window scan orders a message by, newest first. A
    /// provisional author's message dated more than [`PROVISIONAL_TIME_SLACK`]
    /// past the anchor is untrusted: it ranks AT the anchor (no newer than
    /// the newest member message) and behind any message with the same time.
    /// Clamping to `anchor + slack` instead would leave a far-future message
    /// ahead of every honest newcomer for good, because the ceiling moves
    /// with the room. Every site that applies P6 must order by this key.
    pub(crate) fn scan_key(&self, m: &crate::room_state::message::AuthorizedMessageV1) -> ScanKey {
        let (time, id) = (m.message.time, m.id());
        match self.anchor {
            Some(anchor)
                if self.ids.contains(&m.message.author)
                    && time > anchor + PROVISIONAL_TIME_SLACK =>
            {
                (anchor, false, id)
            }
            _ => (time, true, id),
        }
    }
}

/// How far past the anchor a provisional message's time is believed (P6).
/// Ten minutes covers an honest newcomer's clock running a few minutes fast
/// and the delay between signing and a peer receiving the message, while
/// a message dated further ahead is ranked as if it were no newer than the
/// anchor. Bans and member messages are not affected.
pub const PROVISIONAL_TIME_SLACK: std::time::Duration = std::time::Duration::from_secs(600);

/// What ban enforcement fixed for [`ChatRoomStateV1::provisional_step`].
pub(crate) struct BanInputs<'a> {
    /// `resolution.removed`: every id a ban the pass enforces removes.
    pub removed: &'a HashSet<MemberId>,
    /// The targets of the bans that take effect and were issued by the owner
    /// or an owner deputy. Bans by anyone else (the member's inviter, another
    /// ancestor) and ids removed only with a banned inviter are not here.
    pub direct_targets: &'a HashSet<MemberId>,
    /// The records the pass reads for ban judgement (`BansV1::stable_known`).
    pub stable_known: &'a HashMap<MemberId, &'a AuthorizedMember>,
    /// Every key a `members` or evidence record held for an id at the start
    /// of the pass, before evidence deeper than the cap was dropped.
    pub held_record_keys: &'a HashMap<MemberId, Vec<VerifyingKey>>,
}

impl ChatRoomStateV1 {
    /// Cleanup steps P0 to P5 and C3 (river#974 design, 2.4 and 2.5), run in
    /// `cleanup_pass` after ban enforcement and before the member loop. P6 is
    /// the member loop's window scan and P7 runs after it.
    ///
    /// Everything here only removes.
    pub(crate) fn provisional_step(
        &mut self,
        parameters: &ChatRoomParametersV1,
        owner_id: MemberId,
        bans: &BanInputs<'_>,
    ) -> ProvisionalPlan {
        let BanInputs {
            removed,
            direct_targets,
            stable_known,
            held_record_keys,
        } = bans;
        if self.provisional.is_empty() {
            return ProvisionalPlan::default();
        }
        let owner_vk = parameters.owner;

        // The records the pass reads, with every key they hold (the C1 set:
        // members after ban enforcement, plus the evidence known at the start
        // of the pass).
        let mut record_keys: HashMap<MemberId, Vec<VerifyingKey>> = HashMap::new();
        for m in &self.members.members {
            record_keys
                .entry(m.member.id())
                .or_default()
                .push(m.member.member_vk);
        }
        for (id, m) in stable_known.iter() {
            record_keys.entry(*id).or_default().push(m.member.member_vk);
        }

        // The ids whose key is in question: an id with an entry whose key
        // differs from a record's (or the owner's id), or with entries under
        // different keys. Computed over EVERY entry held at the start of the
        // pass, before any rule below drops one, so that whichever rule drops
        // it (a ban, the policy, C1, C2) the content it authored under a key
        // the id no longer resolves to goes too (C3). The records compared
        // are the C1 set and every record held when the pass started
        // (`held_record_keys`, read before evidence deeper than the cap is
        // dropped): the field applies accepted content under any key a held
        // record or admitted entry carried, so every id whose content could
        // have been accepted under a key other than the one it ends with is
        // re-checked here.
        let conflicted: HashSet<MemberId> = {
            let mut keys_by_id: HashMap<MemberId, HashSet<[u8; 32]>> = HashMap::new();
            let mut out = HashSet::new();
            for e in &self.provisional.entries {
                let id = e.member_id();
                let vk = e.provisional.member_vk;
                keys_by_id.entry(id).or_default().insert(vk.to_bytes());
                let differs = |keys: Option<&Vec<VerifyingKey>>| {
                    keys.is_some_and(|keys| keys.iter().any(|k| *k != vk))
                };
                if id == owner_id
                    || differs(record_keys.get(&id))
                    || differs(held_record_keys.get(&id))
                {
                    out.insert(id);
                }
            }
            out.extend(
                keys_by_id
                    .into_iter()
                    .filter(|(_, keys)| keys.len() > 1)
                    .map(|(id, _)| id),
            );
            out
        };

        let config = &self.configuration.configuration;
        // P0
        let Some(policy) = active_policy(config).cloned() else {
            self.provisional = ProvisionalV1::default();
            self.drop_unverifiable(&conflicted, &HashMap::new(), parameters);
            return ProvisionalPlan::default();
        };
        let quota = policy.quota(config.max_recent_messages);

        // P1: provisional bans.
        let kept_bans = {
            let members_by_id = self.members.members_by_member_id();
            let deputies = self.member_info.deputies_of(owner_id);
            let mut seen = HashSet::new();
            let mut valid: Vec<(bool, BanDigest, &AuthorizedProvisionalBan)> = Vec::new();
            for ban in &self.provisional.bans {
                let digest = ban_digest(ban);
                if !seen.insert(digest) {
                    continue;
                }
                let issuer = ban.ban.banned_by;
                let ok = if issuer == owner_id {
                    ban.verify_signature(&owner_vk, &owner_vk).is_ok()
                } else if let (Some(record), true) =
                    (members_by_id.get(&issuer), deputies.contains(&issuer))
                {
                    ban.verify_signature(&owner_vk, &record.member.member_vk)
                        .is_ok()
                } else {
                    false
                };
                if ok {
                    valid.push((issuer != owner_id, digest, ban));
                }
            }
            // Owner bans first, then the first ban per target before repeats,
            // then NEWEST `banned_at` first, then digest. A repeat is a ban
            // whose target an earlier ban in (owner first, newest first,
            // digest) order already names. Newest first keeps the ban just
            // issued when the list is full and cuts the oldest, as UserBans
            // do (`BansV1::enforce_user_ban_cap`); a digest-only order cut a
            // random ban, the new one about 1 time in `cap + 1`. A far-future
            // `banned_at` holds a slot until the cap pushes it out, which is
            // accepted: bans are signed by the owner or an owner deputy, who
            // are trusted. This is the only place the provisional ban set is
            // chosen: summaries and deltas carry `self.provisional.bans`
            // as this step left it.
            valid.sort_by_key(|(non_owner, digest, ban)| {
                (*non_owner, std::cmp::Reverse(ban.ban.banned_at), *digest)
            });
            let mut per_target: HashMap<MemberId, usize> = HashMap::new();
            let mut ranked: Vec<(
                (bool, usize, std::cmp::Reverse<SignedTime>, BanDigest),
                &AuthorizedProvisionalBan,
            )> = valid
                .into_iter()
                .map(|(non_owner, digest, ban)| {
                    let seen_before = per_target.entry(ban.ban.target).or_insert(0);
                    let rank = *seen_before;
                    *seen_before += 1;
                    (
                        (
                            non_owner,
                            rank,
                            std::cmp::Reverse(ban.ban.banned_at),
                            digest,
                        ),
                        ban,
                    )
                })
                .collect();
            ranked.sort_by_key(|(key, _)| *key);
            ranked.truncate(policy.ban_cap());
            let mut kept: Vec<AuthorizedProvisionalBan> =
                ranked.into_iter().map(|(_, b)| b.clone()).collect();
            kept.sort_by_cached_key(ban_digest);
            kept
        };
        let ban_issuers: HashSet<MemberId> = kept_bans
            .iter()
            .map(|b| b.ban.banned_by)
            .filter(|id| *id != owner_id)
            .collect();

        // P2: banned ids. An id the owner or an owner deputy bans directly is
        // banned whatever record holds it: a member they banned does not come
        // back by doing the work, even though their record is ban evidence
        // under the entry's own key. A ban by the member's inviter (or another
        // ancestor) removes the membership but does not block a provisional
        // entry, so a plain member cannot keep a newcomer out by inviting its
        // key and banning it (Ian's decision, 2026-11-22). An id removed only
        // because a ban removed its inviter (`reach_in`) is banned only where
        // no record holds it; where a record holds it under the entry's key,
        // the entry may stay (design D2, consequence (a)), and under another
        // key C1 drops it. A ban on an id no record holds takes effect only
        // from the owner or an owner deputy (`is_ban_authorized` gives such a
        // target the strict ancestors `{owner}`).
        let banned: HashSet<MemberId> = removed
            .iter()
            .filter(|id| !record_keys.contains_key(id) || direct_targets.contains(id))
            .copied()
            .chain(kept_bans.iter().map(|b| b.ban.target))
            .collect();

        let mut entries: Vec<AuthorizedProvisionalMember> =
            std::mem::take(&mut self.provisional.entries);
        entries.retain(|e| !banned.contains(&e.member_id()));
        // P3
        entries.retain(|e| e.check_policy(config, &owner_vk).is_ok());

        // P4: C1, a record beats an entry (and the owner's id is no entry's).
        entries.retain(|e| {
            let id = e.member_id();
            let vk = e.provisional.member_vk;
            !(id == owner_id
                || record_keys
                    .get(&id)
                    .is_some_and(|keys| keys.iter().any(|k| *k != vk)))
        });
        // C2: entries with one id and different keys are all dropped.
        let mut keys_by_id: HashMap<MemberId, HashSet<[u8; 32]>> = HashMap::new();
        for e in &entries {
            keys_by_id
                .entry(e.member_id())
                .or_default()
                .insert(e.provisional.member_vk.to_bytes());
        }
        entries.retain(|e| keys_by_id[&e.member_id()].len() == 1);
        // P5: sponsorship (a members record with the same key), then one entry
        // per key (the smallest digest).
        {
            let members_by_id = self.members.members_by_member_id();
            entries.retain(|e| {
                members_by_id
                    .get(&e.member_id())
                    .is_none_or(|m| m.member.member_vk != e.provisional.member_vk)
            });
        }
        let mut best: HashMap<[u8; 32], EntryDigest> = HashMap::new();
        for e in &entries {
            let d = entry_digest(e);
            best.entry(e.provisional.member_vk.to_bytes())
                .and_modify(|b| *b = (*b).min(d))
                .or_insert(d);
        }
        entries.retain(|e| best[&e.provisional.member_vk.to_bytes()] == entry_digest(e));

        // C3: what a conflicted id authored under a key it no longer
        // resolves to goes with the entry.
        let survivors: HashMap<MemberId, VerifyingKey> = entries
            .iter()
            .map(|e| (e.member_id(), e.provisional.member_vk))
            .collect();
        self.drop_unverifiable(&conflicted, &survivors, parameters);

        let ids: HashSet<MemberId> = entries.iter().map(|e| e.member_id()).collect();
        self.provisional.entries = entries;
        self.provisional.bans = kept_bans;
        // P6 anchor: the newest message of the owner or a member, a pure
        // function of the state at this point (members after ban
        // enforcement), computed once and carried in the plan. With no such
        // message the anchor is the OLDEST provisional message: an empty
        // room has no clock, and this ranks every provisional message dated
        // past it by the slack as untrusted. One provisional author can lower
        // that anchor with an old message, which only ranks the later
        // provisional messages by id until a member speaks.
        let anchor = {
            let member_ids: HashSet<MemberId> =
                self.members.members.iter().map(|m| m.member.id()).collect();
            let msgs = &self.recent_messages.messages;
            msgs.iter()
                .filter(|m| m.message.author == owner_id || member_ids.contains(&m.message.author))
                .map(|m| m.message.time)
                .max()
                .or_else(|| {
                    msgs.iter()
                        .filter(|m| ids.contains(&m.message.author))
                        .map(|m| m.message.time)
                        .min()
                })
        };
        ProvisionalPlan {
            ids,
            quota,
            pool_cap: policy.pool_cap(),
            ban_issuers,
            anchor,
        }
    }
}

impl ChatRoomStateV1 {
    /// C3 (river#974 design, 2.4): held messages and `member_info` records
    /// authored by an id in `conflicted` that do not verify under the key the
    /// id now resolves to are dropped. An id resolves to the owner's key, else
    /// its `members` record's (for `member_info`, a record held as ban
    /// evidence counts), else the key of the surviving entry for it, else to
    /// none. Only conflicted ids are checked, so the cost is one signature
    /// check per held message or record of a conflicted id.
    fn drop_unverifiable(
        &mut self,
        conflicted: &HashSet<MemberId>,
        survivors: &HashMap<MemberId, VerifyingKey>,
        parameters: &ChatRoomParametersV1,
    ) {
        if conflicted.is_empty() {
            return;
        }
        let owner_id = parameters.owner_id();
        let owner_vk = parameters.owner;
        let members_by_id = self.members.members_by_member_id();
        let message_key = |id: MemberId| -> Option<VerifyingKey> {
            if id == owner_id {
                Some(owner_vk)
            } else {
                members_by_id
                    .get(&id)
                    .map(|m| m.member.member_vk)
                    .or_else(|| survivors.get(&id).copied())
            }
        };
        self.recent_messages.messages.retain(|m| {
            !conflicted.contains(&m.message.author)
                || message_key(m.message.author).is_some_and(|k| m.validate(&k).is_ok())
        });
        let known = self.ban_evidence.lookup(&self.members, owner_id);
        self.member_info.member_info.retain(|info| {
            let id = info.member_info.member_id;
            if !conflicted.contains(&id) {
                return true;
            }
            let key = if id == owner_id {
                Some(owner_vk)
            } else {
                known
                    .get(&id)
                    .map(|m| m.member.member_vk)
                    .or_else(|| survivors.get(&id).copied())
            };
            key.is_some_and(|k| info.verify_signature_with_key(&k).is_ok())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier;
    use freenet_scaffold::util::FastHash;
    use std::time::SystemTime;

    fn owner() -> SigningKey {
        SigningKey::from_bytes(&[1u8; 32])
    }

    fn member() -> SigningKey {
        SigningKey::from_bytes(&[2u8; 32])
    }

    fn fixture_entry() -> AuthorizedProvisionalMember {
        AuthorizedProvisionalMember::new(
            &owner().verifying_key(),
            ProvisionalMember {
                member_vk: member().verifying_key(),
                evidence: AdmissionEvidence::Pow { nonce: 12345 },
            },
            &member(),
        )
    }

    fn fixture_ban() -> AuthorizedProvisionalBan {
        AuthorizedProvisionalBan::new(
            &owner().verifying_key(),
            ProvisionalBan {
                banned_by: MemberId::from(&owner().verifying_key()),
                banned_at: (SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000))
                    .into(),
                target: MemberId(FastHash(2)),
            },
            &owner(),
        )
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The proof-of-work hash is wire format (a client and every host must
    /// compute the same value): one fixed input, one fixed output.
    #[test]
    fn pow_hash_golden_vector() {
        let h = pow_hash(
            &owner().verifying_key(),
            &member().verifying_key(),
            7,
            12345,
        );
        assert_eq!(
            hex(&h),
            "84c061284197d17cdbee7d80a2658b8b31f8cc9414e18c04339d50cc9c4fdc26"
        );
        assert_eq!(leading_zero_bits(&h), 0, "0x84 has no leading zero bit");
        // Counted from byte 0, most significant bit first.
        let mut z = [0u8; 32];
        assert_eq!(leading_zero_bits(&z), 256);
        z[0] = 0x01;
        assert_eq!(leading_zero_bits(&z), 7);
        z[0] = 0;
        z[1] = 0x10;
        assert_eq!(leading_zero_bits(&z), 11);
        assert_eq!(POW_DOMAIN_TAG.len(), 27);
        // Each input is bound: epoch, nonce, owner and member each change it.
        let base = pow_hash(&owner().verifying_key(), &member().verifying_key(), 7, 1);
        assert_ne!(
            base,
            pow_hash(&owner().verifying_key(), &member().verifying_key(), 8, 1)
        );
        assert_ne!(
            base,
            pow_hash(&owner().verifying_key(), &member().verifying_key(), 7, 2)
        );
        assert_ne!(
            base,
            pow_hash(&member().verifying_key(), &member().verifying_key(), 7, 1)
        );
        assert_ne!(
            base,
            pow_hash(&owner().verifying_key(), &owner().verifying_key(), 7, 1)
        );
    }

    /// The bytes a member signs, the signature itself (Ed25519 is
    /// deterministic), the entry digest and its CBOR byte-string encoding.
    #[test]
    fn entry_golden_vectors() {
        let e = fixture_entry();
        let o = owner().verifying_key();
        let bytes = provisional_signing_bytes(&o, &e.provisional);
        assert_eq!(
            hex(&bytes),
            "72697665722d70726f766973696f6e616c2d6d656d6265722f76318a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5ca2696d656d6265725f766b58208139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b3946865766964656e6365a163506f77a1656e6f6e6365193039"
        );
        assert!(
            bytes.starts_with(b"river-provisional-member/v1"),
            "the domain tag leads, so a vault can refuse non-River callers on it"
        );
        assert_eq!(
            hex(&e.signature.to_bytes()),
            "a0d09126e99f1c1e6b07c8f19f16cbe403775ccbe2210ea5a73ee5fd07036aa7a1c9a9fee43e007852b269b346acf274d500e29155fd0c426570d037112d400c"
        );
        let digest = entry_digest(&e);
        assert_eq!(hex(&digest.0), "b095e5cac73faaaa073a64caa80c4a16");
        let mut reversed = digest.0;
        reversed.reverse();
        assert_ne!(digest.0, reversed, "the vector must distinguish byte order");
        let encoded = cbor(&digest);
        assert_eq!(encoded.len(), 17, "a CBOR byte string of 16 bytes");
        assert_eq!(encoded[0], 0x50);
        assert_eq!(&encoded[1..], &digest.0);
        // The digest is over the whole record, signature included.
        assert_eq!(&crate::util::record_digest(&e)[..16], &digest.0);
        assert!(e.verify_signature(&o).is_ok());
        // A different owner key does not verify it: the owner is bound in.
        assert!(e.verify_signature(&member().verifying_key()).is_err());
        // The summary pair encodes as a 2-element array of the id and a byte
        // string, not a 16-element array.
        let pair = (e.member_id(), digest);
        let pair_bytes = cbor(&pair);
        assert!(
            pair_bytes.len() <= 1 + 10 + 17,
            "an entry's summary pair stays small: {} bytes",
            pair_bytes.len()
        );
    }

    #[test]
    fn ban_golden_vectors() {
        let b = fixture_ban();
        let o = owner().verifying_key();
        let bytes = provisional_ban_signing_bytes(&o, &b.ban);
        assert_eq!(
            hex(&bytes),
            "72697665722d70726f766973696f6e616c2d62616e2f76318a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5ca36962616e6e65645f62791b20340a250a45d4606962616e6e65645f6174a270736563735f73696e63655f65706f63681a6553f100716e616e6f735f73696e63655f65706f6368006674617267657402"
        );
        assert_eq!(
            hex(&b.signature.to_bytes()),
            "e6e6c777684f72e655b39c49fa5173410197e79b984625f6121ac21acbb76745aa3a666fbbd9305a9ca8815fbb9724366cb2e01efa0c06196473fdaff40d4305"
        );
        let digest = ban_digest(&b);
        assert_eq!(hex(&digest.0), "248e18100990133c9a202767898d327e");
        let mut reversed = digest.0;
        reversed.reverse();
        assert_ne!(digest.0, reversed);
        assert_eq!(hex(&cbor(&digest)), "50248e18100990133c9a202767898d327e");
        assert!(b.verify_signature(&o, &o).is_ok());
        // Not replayable as a `UserBan`, nor an entry's signature as a ban's:
        // the signed bytes start with different tags.
        assert!(bytes.starts_with(b"river-provisional-ban/v1"));
        assert!(!bytes.starts_with(ENTRY_SIGNING_TAG));
        let as_user_ban = crate::room_state::ban::UserBan {
            owner_member_id: MemberId::from(&o),
            banned_at: b.ban.banned_at,
            banned_user: b.ban.target,
        };
        let user_ban_bytes = cbor(&as_user_ban);
        assert!(owner()
            .verifying_key()
            .verify(&user_ban_bytes, &b.signature)
            .is_err());
    }

    /// The wire shape of the unsigned records is part of the contract.
    #[test]
    fn provisional_records_round_trip_through_cbor() {
        let state = ProvisionalV1 {
            entries: vec![fixture_entry()],
            bans: vec![fixture_ban()],
        };
        let back: ProvisionalV1 = ciborium::de::from_reader(cbor(&state).as_slice()).unwrap();
        assert_eq!(back, state);
        assert!(ProvisionalV1::default().is_empty());
        assert!(unhex("00").len() == 1);
    }

    #[test]
    fn policy_values_are_absolute_ranges() {
        let ok = PublicRoomPolicy {
            listed: false,
            max_provisional: 16,
            max_provisional_messages: 500,
            max_provisional_bans: 16,
            pow: Some(PowPolicy {
                difficulty_bits: 32,
                epoch: 0,
            }),
        };
        assert!(ok.check_values().is_ok());
        let mut p = ok.clone();
        p.max_provisional = 0;
        assert!(p.check_values().is_err());
        p.max_provisional = 17;
        assert!(p.check_values().is_err());
        let mut p = ok.clone();
        p.max_provisional_messages = 0;
        assert!(p.check_values().is_err());
        p.max_provisional_messages = 501;
        assert!(p.check_values().is_err());
        let mut p = ok.clone();
        p.max_provisional_bans = 17;
        assert!(p.check_values().is_err());
        p.max_provisional_bans = 0;
        assert!(p.check_values().is_ok());
        let mut p = ok.clone();
        p.pow = Some(PowPolicy {
            difficulty_bits: 33,
            epoch: 0,
        });
        assert!(p.check_values().is_err());
        p.pow = None;
        assert!(
            p.check_values().is_ok(),
            "no pow admits nobody and is valid"
        );
        // The quota is clamped at use, never checked against another field.
        assert_eq!(ok.quota(10), 5);
        assert_eq!(ok.quota(1), 0, "with max_recent_messages < 2 nobody posts");
        assert_eq!(ok.quota(10_000), 500);
    }

    /// C3's start-of-pass rule, through `provisional_step` with a hand-built
    /// `held_record_keys`: an id with no record under another key is not
    /// conflicted, but an id that held a record under ANOTHER key at the start
    /// of the pass (the record the merge then dropped as too deep) is, and the
    /// content it authored under that other key goes. The message below
    /// verifies under Y's key, not under the entry's key, so only the
    /// conflict makes it go.
    #[test]
    fn c3_reads_the_records_held_at_the_start_of_the_pass() {
        use crate::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
        use crate::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};

        let owner_sk = owner();
        let params = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let owner_id = params.owner_id();
        let entry = fixture_entry();
        let x_id = entry.member_id();
        let y = SigningKey::from_bytes(&[9u8; 32]);
        let config = Configuration {
            owner_member_id: owner_id,
            public_admission: Some(PublicRoomPolicy {
                listed: false,
                max_provisional: 3,
                max_provisional_messages: 5,
                max_provisional_bans: 3,
                pow: Some(PowPolicy {
                    difficulty_bits: 0,
                    epoch: 0,
                }),
            }),
            ..Configuration::default()
        };
        let build = || {
            let mut state = ChatRoomStateV1 {
                configuration: AuthorizedConfigurationV1::new(config.clone(), &owner_sk),
                ..Default::default()
            };
            state.provisional.entries.push(entry.clone());
            // Authored under Y's key with X's id: not valid under the entry.
            state
                .recent_messages
                .messages
                .push(AuthorizedMessageV1::new(
                    MessageV1 {
                        room_owner: owner_id,
                        author: x_id,
                        time: (SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(5)).into(),
                        content: RoomMessageBody::public("under another key".to_string()),
                    },
                    &y,
                ));
            state
        };
        let run = |held: HashMap<MemberId, Vec<VerifyingKey>>| {
            let mut state = build();
            let removed = HashSet::new();
            let direct = HashSet::new();
            let stable: HashMap<MemberId, &crate::room_state::member::AuthorizedMember> =
                HashMap::new();
            let plan = state.provisional_step(
                &params,
                owner_id,
                &BanInputs {
                    removed: &removed,
                    direct_targets: &direct,
                    stable_known: &stable,
                    held_record_keys: &held,
                },
            );
            assert!(plan.ids.contains(&x_id), "the entry stands either way");
            state.recent_messages.messages.len()
        };
        assert_eq!(
            run(HashMap::new()),
            1,
            "no record held another key: not conflicted, the content is left alone"
        );
        let mut held = HashMap::new();
        held.insert(x_id, vec![y.verifying_key()]);
        assert_eq!(
            run(held),
            0,
            "a record under another key held at the start of the pass conflicts the id"
        );
    }

    /// A provisional ban signature is checked with `verify_strict`, like an
    /// entry's: a small-order issuer key with the identity `R` and `S = 0`
    /// passes the cofactored `verify` for any message, and must not pass here.
    #[test]
    fn a_provisional_ban_signature_under_a_small_order_key_is_refused() {
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let weak = VerifyingKey::from_bytes(&identity).expect("the identity point decodes");
        assert!(weak.is_weak(), "premise: the key is small order");
        let mut sig_bytes = [0u8; 64];
        sig_bytes[0] = 1; // R = identity, S = 0
        let signature = Signature::from_bytes(&sig_bytes);
        let mut b = fixture_ban();
        b.signature = signature;
        assert!(
            weak.verify(
                &provisional_ban_signing_bytes(&owner().verifying_key(), &b.ban),
                &signature
            )
            .is_ok(),
            "premise: the non-strict check accepts it"
        );
        assert!(b.verify_signature(&owner().verifying_key(), &weak).is_err());
    }
}
