//! Client API for open admission to public rooms (freenet/river#974, Phase 1).
//!
//! The web UI and riverctl call only this module, so a UI can build against it
//! (or a mock of it) before the contract ships. Nothing here is compiled into
//! the room contract or the chat delegate: the module is behind the
//! `admission` feature, enabled by client crates only.
//!
//! The pieces:
//! - [`admission_requirements`]: what a room asks of a newcomer.
//! - [`admission_status`]: where this key stands in the room, and why it needs
//!   (re-)admission if it does.
//! - [`PowSolver`]: a cooperative, allocation-free proof-of-work search the UI
//!   runs in slices and riverctl runs one per core.
//! - [`provisional_signing_bytes`] / [`assemble_provisional`]: the entry is
//!   built with the member key passed in nowhere, so the signature can come
//!   from whoever holds the key (the UI's local key today, `freenet-vault` once
//!   River's key custody moves there).
//! - [`admission_delta`] / [`check_admission`]: the delta that carries an entry
//!   with the member's `member_info` and first message, and the contract's own
//!   predicates with the reason.
//!
//! `freenet-vault` requirement: the signing bytes of an entry start with
//! `river-provisional-member/v1`. The vault's caller check must stop a
//! non-River caller from getting a River member key to sign them, as for
//! River's other domain tags.

use crate::room_state::configuration::Configuration;
use crate::room_state::member::MemberId;
use crate::room_state::member_info::AuthorizedMemberInfo;
use crate::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use crate::room_state::privacy::PrivacyMode;
use crate::room_state::provisional::{
    active_policy, entry_digest, leading_zero_bits, pow_hash, AdmissionEvidence,
    AuthorizedProvisionalMember, ProvisionalDelta, ProvisionalMember, MAX_DIFFICULTY_BITS,
};
use crate::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use ed25519_dalek::{Signature, VerifyingKey};
use std::time::SystemTime;

pub use crate::room_state::provisional::{
    pow_hash as admission_pow_hash, provisional_signing_bytes, AdmissionError, PowPolicy,
    PublicRoomPolicy,
};

/// A proof-of-work requirement, with the expected cost of meeting it.
#[derive(Debug, Clone, PartialEq)]
pub struct PowRequirement {
    pub difficulty_bits: u8,
    pub epoch: u32,
    /// `2^difficulty_bits`: the hashes a solver expects to try.
    pub expected_hashes: f64,
}

/// What a room asks of a newcomer.
#[derive(Debug, Clone, PartialEq)]
pub enum AdmissionRequirements {
    /// Members join by invitation only (a private room, or no open-admission
    /// policy).
    InviteOnly,
    Open {
        /// `None`: the policy admits nobody (a listed, invite-only room).
        pow: Option<PowRequirement>,
        /// Provisional authors the room holds at once.
        max_provisional: u32,
        /// Window slots provisional messages may fill
        /// (`min(max_provisional_messages, max_recent_messages / 2)`); the
        /// owner UI says so when this is 0.
        quota: u32,
        /// Atlas discovery opt-in; no effect on admission.
        listed: bool,
    },
}

/// What `config` asks of a newcomer.
pub fn admission_requirements(config: &Configuration) -> AdmissionRequirements {
    match active_policy(config) {
        None => AdmissionRequirements::InviteOnly,
        Some(policy) => AdmissionRequirements::Open {
            pow: policy.pow.as_ref().map(|p| PowRequirement {
                difficulty_bits: p.difficulty_bits,
                epoch: p.epoch,
                expected_hashes: 2f64.powi(p.difficulty_bits as i32),
            }),
            max_provisional: policy.max_provisional,
            quota: policy.quota(config.max_recent_messages) as u32,
            listed: policy.listed,
        },
    }
}

/// Where a key stands in a room.
#[derive(Debug, Clone, PartialEq)]
pub enum AdmissionStatus {
    Owner,
    /// Has a `members` record for this key.
    Member,
    /// The room holds an entry for this key.
    Provisional,
    /// A ban the room enforces covers this id, or a provisional ban names it.
    Banned,
    NeedsAdmission {
        reason: NeedsAdmissionReason,
        requirements: AdmissionRequirements,
    },
}

/// Why a key needs (re-)admission.
#[derive(Debug, Clone, PartialEq)]
pub enum NeedsAdmissionReason {
    /// The client holds no entry.
    NeverAdmitted,
    /// The stored entry fails the room's current policy: re-solve.
    PolicyChanged(AdmissionError),
    /// The stored entry is valid but the room holds no message of this
    /// author: attach it to the next message.
    Pruned,
    /// The stored entry is valid, but the pool or the quota is held by newer
    /// messages: re-solving will not help.
    RoomFull,
    /// A `members` or ban-evidence record holds this id with another key: ask
    /// the owner, or use a new key.
    Shadowed,
}

/// Where `member_vk` stands in the room.
///
/// `stored` is the entry the client holds for this key, if any. `Pruned`
/// versus `RoomFull` is decided by running the room's own cleanup on a copy of
/// the state that holds the entry and a message stamped `send_time`, the time
/// the client will actually put on it, so future-dated messages holding the
/// quota correctly give `RoomFull`.
pub fn admission_status(
    state: &ChatRoomStateV1,
    params: &ChatRoomParametersV1,
    member_vk: &VerifyingKey,
    stored: Option<&AuthorizedProvisionalMember>,
    send_time: SystemTime,
) -> AdmissionStatus {
    let id = MemberId::from(member_vk);
    let owner_id = params.owner_id();
    if *member_vk == params.owner {
        return AdmissionStatus::Owner;
    }
    let config = &state.configuration.configuration;
    let known = state.ban_evidence.lookup(&state.members, owner_id);
    if let Some(record) = state.members.members_by_member_id().get(&id) {
        return if record.member.member_vk == *member_vk {
            AdmissionStatus::Member
        } else {
            needs(NeedsAdmissionReason::Shadowed, config)
        };
    }
    if state
        .provisional
        .entries
        .iter()
        .any(|e| e.provisional.member_vk == *member_vk)
    {
        return AdmissionStatus::Provisional;
    }
    let banned = state.provisional.bans.iter().any(|b| b.ban.target == id)
        || crate::room_state::direct_messages::enforced_ban_set_of(state, params).contains(&id);
    if banned {
        return AdmissionStatus::Banned;
    }
    if known
        .get(&id)
        .is_some_and(|r| r.member.member_vk != *member_vk)
        || id == owner_id
    {
        return needs(NeedsAdmissionReason::Shadowed, config);
    }
    let Some(entry) = stored else {
        return needs(NeedsAdmissionReason::NeverAdmitted, config);
    };
    if let Err(e) = check_admission(state, params, entry) {
        return needs(NeedsAdmissionReason::PolicyChanged(e), config);
    }
    // The entry is valid: would the room keep it with a message stamped
    // `send_time`? Ask the room's own cleanup, on a copy.
    let mut probe = state.clone();
    probe.provisional.entries.push(entry.clone());
    probe.recent_messages.messages.push(AuthorizedMessageV1 {
        message: MessageV1 {
            room_owner: owner_id,
            author: id,
            time: send_time,
            content: RoomMessageBody::public(String::new()),
        },
        // Cleanup does not check a message's signature unless its author's key
        // is in question, and the key of this id is not (checked above).
        signature: Signature::from_bytes(&[0u8; 64]),
    });
    let kept = probe.post_apply_cleanup(params).is_ok()
        && probe
            .provisional
            .entries
            .iter()
            .any(|e| e.provisional.member_vk == *member_vk);
    let reason = if kept {
        NeedsAdmissionReason::Pruned
    } else {
        NeedsAdmissionReason::RoomFull
    };
    needs(reason, config)
}

fn needs(reason: NeedsAdmissionReason, config: &Configuration) -> AdmissionStatus {
    AdmissionStatus::NeedsAdmission {
        reason,
        requirements: admission_requirements(config),
    }
}

/// The contract's own predicates for `entry` in `state`'s room, with the
/// reason: its self-signature verifies, and it admits under the room's
/// current policy.
pub fn check_admission(
    state: &ChatRoomStateV1,
    params: &ChatRoomParametersV1,
    entry: &AuthorizedProvisionalMember,
) -> Result<(), AdmissionError> {
    entry
        .verify_signature(&params.owner)
        .map_err(|_| AdmissionError::BadSignature)?;
    entry.check_policy(&state.configuration.configuration, &params.owner)
}

/// Build an entry from its parts: the signature may come from whoever holds
/// the member's key.
pub fn assemble_provisional(
    member: ProvisionalMember,
    signature: Signature,
) -> AuthorizedProvisionalMember {
    AuthorizedProvisionalMember::with_signature(member, signature)
}

/// The delta that admits a provisional member: the entry (left out when the
/// room already holds it), the member's `member_info` and their first message,
/// in one delta, so the contract accepts the message with the entry.
pub fn admission_delta(
    state: &ChatRoomStateV1,
    _params: &ChatRoomParametersV1,
    entry: &AuthorizedProvisionalMember,
    member_info: AuthorizedMemberInfo,
    message: AuthorizedMessageV1,
) -> ChatRoomStateV1Delta {
    let held = entry_digest(entry);
    let id = entry.member_id();
    let already_held = state
        .provisional
        .entries
        .iter()
        .any(|e| e.member_id() == id && entry_digest(e) == held);
    ChatRoomStateV1Delta {
        provisional: (!already_held).then(|| ProvisionalDelta {
            entries: vec![entry.clone()],
            bans: vec![],
        }),
        member_info: Some(vec![member_info]),
        recent_messages: Some(vec![message]),
        ..Default::default()
    }
}

/// The proof-of-work search. Cooperative and allocation-free: [`Self::step`]
/// does a bounded number of hashes and returns, so the UI can run it in slices
/// between frames and riverctl can run one solver per core
/// ([`Self::with_partition`]). Cancelling is dropping the solver.
#[derive(Debug, Clone)]
pub struct PowSolver {
    owner_vk: VerifyingKey,
    member_vk: VerifyingKey,
    epoch: u32,
    target_bits: u8,
    start: u64,
    stride: u64,
    next: u64,
    tried: u64,
}

/// One slice of a [`PowSolver`].
#[derive(Debug, Clone, PartialEq)]
pub enum PowStep {
    /// A nonce reaching the target. `achieved_bits >= target_bits`.
    Found { nonce: u64, achieved_bits: u8 },
    /// Not yet: hashes tried in total, and the expected number needed.
    Progress { tried: u64, expected: f64 },
}

/// Highest target a [`PowSolver`] accepts: the contract's difficulty ceiling
/// plus a few bits of over-solving. A higher request is clamped, since a
/// target of 40 or more bits does not finish on any machine.
pub const MAX_TARGET_BITS: u8 = MAX_DIFFICULTY_BITS + 8;

impl PowSolver {
    /// A solver for `req` aiming at `target_bits` (at least the required
    /// bits: a client may over-solve by a bit or two, so a small raise of the
    /// difficulty does not invalidate its proof).
    pub fn new(
        owner_vk: &VerifyingKey,
        member_vk: &VerifyingKey,
        req: &PowRequirement,
        target_bits: u8,
    ) -> Self {
        PowSolver {
            owner_vk: *owner_vk,
            member_vk: *member_vk,
            epoch: req.epoch,
            target_bits: target_bits.max(req.difficulty_bits).min(MAX_TARGET_BITS),
            start: 0,
            stride: 1,
            next: 0,
            tried: 0,
        }
    }

    /// Search nonces `start, start + stride, start + 2 * stride, ...`: with
    /// `stride` solvers started at `0..stride`, each nonce is tried once.
    pub fn with_partition(mut self, start: u64, stride: u64) -> Self {
        self.start = start;
        self.stride = stride.max(1);
        self.next = start;
        self
    }

    /// Follow a changed requirement. The same epoch continues (a nonce already
    /// tried that missed the old target misses a higher one too); a new epoch
    /// restarts, since no earlier work counts.
    pub fn retarget(&mut self, req: &PowRequirement, target_bits: u8) {
        self.target_bits = target_bits.max(req.difficulty_bits).min(MAX_TARGET_BITS);
        if req.epoch != self.epoch {
            self.epoch = req.epoch;
            self.next = self.start;
            self.tried = 0;
        }
    }

    /// Try up to `budget` nonces.
    pub fn step(&mut self, budget: u64) -> PowStep {
        for _ in 0..budget {
            let nonce = self.next;
            self.next = self.next.wrapping_add(self.stride);
            self.tried += 1;
            let bits = leading_zero_bits(&pow_hash(
                &self.owner_vk,
                &self.member_vk,
                self.epoch,
                nonce,
            ));
            if bits >= self.target_bits as u32 {
                return PowStep::Found {
                    nonce,
                    achieved_bits: bits.min(255) as u8,
                };
            }
        }
        PowStep::Progress {
            tried: self.tried,
            expected: 2f64.powi(self.target_bits as i32),
        }
    }

    /// The `ProvisionalMember` for a nonce this solver found.
    pub fn member(&self, nonce: u64) -> ProvisionalMember {
        ProvisionalMember {
            member_vk: self.member_vk,
            evidence: AdmissionEvidence::Pow { nonce },
        }
    }
}

/// Whether `config` is `Public` with a policy that admits by proof of work.
pub fn admits_by_pow(config: &Configuration) -> bool {
    config.privacy_mode == PrivacyMode::Public
        && config
            .public_admission
            .as_ref()
            .is_some_and(|p| p.pow.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::configuration::AuthorizedConfigurationV1;
    use crate::room_state::member_info::MemberInfo;
    use ed25519_dalek::SigningKey;

    fn owner() -> SigningKey {
        SigningKey::from_bytes(&[200; 32])
    }

    fn room(policy: Option<PublicRoomPolicy>) -> (ChatRoomStateV1, ChatRoomParametersV1) {
        let params = ChatRoomParametersV1 {
            owner: owner().verifying_key(),
        };
        let config = Configuration {
            owner_member_id: params.owner_id(),
            max_recent_messages: 10,
            public_admission: policy,
            ..Default::default()
        };
        (
            ChatRoomStateV1 {
                configuration: AuthorizedConfigurationV1::new(config, &owner()),
                ..Default::default()
            },
            params,
        )
    }

    fn open(bits: u8) -> PublicRoomPolicy {
        PublicRoomPolicy {
            listed: true,
            max_provisional: 2,
            max_provisional_messages: 3,
            max_provisional_bans: 2,
            pow: Some(PowPolicy {
                difficulty_bits: bits,
                epoch: 4,
            }),
        }
    }

    fn requirement(state: &ChatRoomStateV1) -> PowRequirement {
        match admission_requirements(&state.configuration.configuration) {
            AdmissionRequirements::Open { pow: Some(p), .. } => p,
            other => panic!("not an open room: {other:?}"),
        }
    }

    fn solve(
        state: &ChatRoomStateV1,
        params: &ChatRoomParametersV1,
        k: &SigningKey,
        extra_bits: u8,
    ) -> AuthorizedProvisionalMember {
        let req = requirement(state);
        let mut solver = PowSolver::new(
            &params.owner,
            &k.verifying_key(),
            &req,
            req.difficulty_bits + extra_bits,
        );
        loop {
            if let PowStep::Found { nonce, .. } = solver.step(1_000) {
                let member = solver.member(nonce);
                let sig = ed25519_dalek::Signer::sign(
                    k,
                    &provisional_signing_bytes(&params.owner, &member),
                );
                return assemble_provisional(member, sig);
            }
        }
    }

    fn message(k: &SigningKey, owner_id: MemberId, at: u64) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: MemberId::from(&k.verifying_key()),
                time: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(at),
                content: RoomMessageBody::public("hi".into()),
            },
            k,
        )
    }

    fn info(k: &SigningKey) -> AuthorizedMemberInfo {
        AuthorizedMemberInfo::new_with_member_key(
            MemberInfo::new_public(MemberId::from(&k.verifying_key()), 1, "n".into()),
            k,
        )
    }

    #[test]
    fn requirements_follow_the_configuration() {
        let (s, _) = room(None);
        assert_eq!(
            admission_requirements(&s.configuration.configuration),
            AdmissionRequirements::InviteOnly
        );
        let (s, _) = room(Some(open(10)));
        assert_eq!(
            admission_requirements(&s.configuration.configuration),
            AdmissionRequirements::Open {
                pow: Some(PowRequirement {
                    difficulty_bits: 10,
                    epoch: 4,
                    expected_hashes: 1024.0
                }),
                max_provisional: 2,
                quota: 3,
                listed: true,
            }
        );
        let mut nopow = open(10);
        nopow.pow = None;
        let (s, _) = room(Some(nopow));
        assert!(matches!(
            admission_requirements(&s.configuration.configuration),
            AdmissionRequirements::Open { pow: None, .. }
        ));
        assert!(admits_by_pow(
            &room(Some(open(1))).0.configuration.configuration
        ));
        assert!(!admits_by_pow(
            &room(Some(nopow_policy())).0.configuration.configuration
        ));
    }

    fn nopow_policy() -> PublicRoomPolicy {
        let mut p = open(1);
        p.pow = None;
        p
    }

    /// The solver finds a proof the contract accepts, in slices, and a
    /// partitioned set of solvers covers disjoint nonces.
    #[test]
    fn the_solver_finds_proofs_the_contract_accepts() {
        let (state, params) = room(Some(open(6)));
        let k = SigningKey::from_bytes(&[5; 32]);
        let entry = solve(&state, &params, &k, 0);
        assert_eq!(check_admission(&state, &params, &entry), Ok(()));
        // A step too small to find anything reports progress.
        let req = requirement(&state);
        let mut s = PowSolver::new(&params.owner, &k.verifying_key(), &req, 24);
        match s.step(10) {
            PowStep::Progress { tried, expected } => {
                assert_eq!(tried, 10);
                assert_eq!(expected, 16_777_216.0);
            }
            PowStep::Found { .. } => panic!("24 bits in 10 hashes"),
        }
        // Partitioned solvers try different nonces.
        let a = PowSolver::new(&params.owner, &k.verifying_key(), &req, 6).with_partition(0, 2);
        let b = PowSolver::new(&params.owner, &k.verifying_key(), &req, 6).with_partition(1, 2);
        let first = |mut s: PowSolver| loop {
            if let PowStep::Found { nonce, .. } = s.step(100) {
                return nonce;
            }
        };
        let (na, nb) = (first(a), first(b));
        assert_eq!(na % 2, 0);
        assert_eq!(nb % 2, 1);
    }

    #[test]
    fn retarget_keeps_progress_in_an_epoch_and_restarts_in_a_new_one() {
        let (state, params) = room(Some(open(4)));
        let k = SigningKey::from_bytes(&[5; 32]);
        let req = requirement(&state);
        let mut s = PowSolver::new(&params.owner, &k.verifying_key(), &req, 30);
        s.step(50);
        let mut raised = req.clone();
        raised.difficulty_bits = 8;
        s.retarget(&raised, 8);
        match s.step(1) {
            PowStep::Progress { tried, .. } => assert_eq!(tried, 51, "same epoch continues"),
            PowStep::Found { .. } => {}
        }
        let mut new_epoch = req.clone();
        new_epoch.epoch = 99;
        s.retarget(&new_epoch, 30);
        match s.step(1) {
            PowStep::Progress { tried, .. } => assert_eq!(tried, 1, "a new epoch restarts"),
            PowStep::Found { .. } => panic!("30 bits"),
        }
        // The target is never below the requirement.
        let low = PowSolver::new(&params.owner, &k.verifying_key(), &req, 1);
        assert_eq!(low.target_bits, 4);
    }

    /// The status walks the cases the UI must tell apart.
    #[test]
    fn status_names_why_a_key_needs_admission() {
        let (mut state, params) = room(Some(open(2)));
        let k = SigningKey::from_bytes(&[5; 32]);
        let owner_id = params.owner_id();
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let status = |state: &ChatRoomStateV1, stored: Option<&AuthorizedProvisionalMember>| {
            admission_status(state, &params, &k.verifying_key(), stored, now)
        };
        assert_eq!(
            admission_status(&state, &params, &params.owner, None, now),
            AdmissionStatus::Owner
        );
        assert!(matches!(
            status(&state, None),
            AdmissionStatus::NeedsAdmission {
                reason: NeedsAdmissionReason::NeverAdmitted,
                ..
            }
        ));
        // A valid stored entry, no message in the window: Pruned.
        let entry = solve(&state, &params, &k, 0);
        assert!(matches!(
            status(&state, Some(&entry)),
            AdmissionStatus::NeedsAdmission {
                reason: NeedsAdmissionReason::Pruned,
                ..
            }
        ));
        // The admission delta makes the room hold the entry: Provisional.
        let delta = admission_delta(
            &state,
            &params,
            &entry,
            info(&k),
            message(&k, owner_id, 1_000),
        );
        use freenet_scaffold::ComposableState;
        let parent = state.clone();
        state.apply_delta(&parent, &params, &Some(delta)).unwrap();
        assert_eq!(status(&state, Some(&entry)), AdmissionStatus::Provisional);
        // A stored entry that no longer meets the policy: PolicyChanged.
        let mut raised = state.configuration.configuration.clone();
        raised.configuration_version += 1;
        raised.public_admission = Some(open(20));
        let mut fresh = state.clone();
        fresh.configuration = AuthorizedConfigurationV1::new(raised, &owner());
        fresh.post_apply_cleanup(&params).unwrap();
        assert!(matches!(
            admission_status(&fresh, &params, &k.verifying_key(), Some(&entry), now),
            AdmissionStatus::NeedsAdmission {
                reason: NeedsAdmissionReason::PolicyChanged(
                    AdmissionError::InsufficientWork { .. }
                ),
                ..
            }
        ));
        // A banned key: Banned.
        let target = MemberId::from(&k.verifying_key());
        let ban = crate::room_state::provisional::AuthorizedProvisionalBan::new(
            &params.owner,
            crate::room_state::provisional::ProvisionalBan {
                banned_by: owner_id,
                banned_at: now,
                target,
            },
            &owner(),
        );
        let mut banned = state.clone();
        let d = ChatRoomStateV1Delta {
            provisional: Some(ProvisionalDelta {
                entries: vec![],
                bans: vec![ban],
            }),
            ..Default::default()
        };
        let parent = banned.clone();
        banned.apply_delta(&parent, &params, &Some(d)).unwrap();
        assert_eq!(
            admission_status(&banned, &params, &k.verifying_key(), Some(&entry), now),
            AdmissionStatus::Banned
        );
    }

    /// `RoomFull`: valid entry, but the pool or quota is held by newer
    /// messages, including future-dated ones.
    #[test]
    fn a_future_dated_flood_gives_room_full_not_pruned() {
        let (mut state, params) = room(Some(open(0)));
        let owner_id = params.owner_id();
        use freenet_scaffold::ComposableState;
        // Two provisional authors fill the pool (max_provisional 2) with
        // future-dated messages.
        for i in 0..2u8 {
            let k = SigningKey::from_bytes(&[50 + i; 32]);
            let entry = solve(&state, &params, &k, 0);
            let d = admission_delta(
                &state,
                &params,
                &entry,
                info(&k),
                message(&k, owner_id, 5_000_000 + i as u64),
            );
            let parent = state.clone();
            state.apply_delta(&parent, &params, &Some(d)).unwrap();
        }
        assert_eq!(state.provisional.entries.len(), 2);
        let k = SigningKey::from_bytes(&[9; 32]);
        let entry = solve(&state, &params, &k, 0);
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        assert!(matches!(
            admission_status(&state, &params, &k.verifying_key(), Some(&entry), now),
            AdmissionStatus::NeedsAdmission {
                reason: NeedsAdmissionReason::RoomFull,
                ..
            }
        ));
        // Stamped newer than the flood, it would be taken: Pruned.
        let later = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(9_000_000);
        assert!(matches!(
            admission_status(&state, &params, &k.verifying_key(), Some(&entry), later),
            AdmissionStatus::NeedsAdmission {
                reason: NeedsAdmissionReason::Pruned,
                ..
            }
        ));
    }

    #[test]
    fn an_entry_held_already_is_not_sent_again() {
        let (mut state, params) = room(Some(open(1)));
        let k = SigningKey::from_bytes(&[5; 32]);
        let owner_id = params.owner_id();
        let entry = solve(&state, &params, &k, 0);
        let d = admission_delta(&state, &params, &entry, info(&k), message(&k, owner_id, 1));
        assert!(d.provisional.is_some());
        use freenet_scaffold::ComposableState;
        let parent = state.clone();
        state.apply_delta(&parent, &params, &Some(d)).unwrap();
        let d = admission_delta(&state, &params, &entry, info(&k), message(&k, owner_id, 2));
        assert!(d.provisional.is_none());
    }

    #[test]
    fn the_solver_clamps_its_target() {
        let owner = SigningKey::from_bytes(&[1; 32]).verifying_key();
        let member = SigningKey::from_bytes(&[2; 32]).verifying_key();
        let req = PowRequirement {
            difficulty_bits: 1,
            epoch: 0,
            expected_hashes: 2.0,
        };
        let mut s = PowSolver::new(&owner, &member, &req, 255);
        assert_eq!(s.target_bits, MAX_TARGET_BITS);
        s.retarget(&req, 200);
        assert_eq!(s.target_bits, MAX_TARGET_BITS);
    }
}
