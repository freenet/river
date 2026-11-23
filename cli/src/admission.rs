//! riverctl's side of open admission to public rooms (freenet/river#974).
//!
//! `riverctl room admission <room>` reads what a room asks of a newcomer,
//! `riverctl room join <room> --pow` solves the proof of work and posts the
//! entry with the member's `member_info` and first message, and the explicit
//! signing-key send path attaches a stored entry when the room pruned it.
//! The rules live in `river_core::room_state::admission`; this module only
//! drives them from the command line.

use crate::storage::Storage;
use anyhow::{anyhow, bail, Result};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use river_core::room_state::admission::{
    admission_delta, admission_requirements, admission_status, assemble_provisional,
    provisional_signing_bytes, AdmissionRequirements, AdmissionStatus, NeedsAdmissionReason,
    PowRequirement, PowSolver, PowStep,
};
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::provisional::{
    AuthorizedProvisionalMember, ProvisionalDelta, PublicRoomPolicy, MAX_DIFFICULTY_BITS,
    MAX_PROVISIONAL_BANS_CEILING, MAX_PROVISIONAL_CEILING, MAX_PROVISIONAL_MESSAGES_CEILING,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, SystemTime};

/// A human description of what a room asks of a newcomer.
pub fn describe_requirements(req: &AdmissionRequirements) -> String {
    match req {
        AdmissionRequirements::InviteOnly => {
            "invite only: accept an invitation with `riverctl invite accept`".to_string()
        }
        AdmissionRequirements::Open {
            pow: None,
            max_provisional,
            ..
        } => format!(
            "listed, but admits nobody by proof of work (the room holds at most \
             {max_provisional} open members at once); ask for an invitation"
        ),
        AdmissionRequirements::Open {
            pow: Some(p),
            max_provisional,
            quota,
            listed,
        } => format!(
            "open: join with `riverctl room join <room> --pow`; proof of work {} bits \
             (about {:.0} hashes), epoch {}; the room holds at most {max_provisional} open \
             members, who may fill {quota} message slots{}",
            p.difficulty_bits,
            p.expected_hashes,
            p.epoch,
            if *listed {
                "; listed for discovery"
            } else {
                ""
            }
        ),
    }
}

/// The requirements as JSON.
pub fn requirements_json(req: &AdmissionRequirements) -> serde_json::Value {
    match req {
        AdmissionRequirements::InviteOnly => json!({"mode": "invite_only"}),
        AdmissionRequirements::Open {
            pow,
            max_provisional,
            quota,
            listed,
        } => json!({
            "mode": "open",
            "pow": pow.as_ref().map(|p| json!({
                "difficulty_bits": p.difficulty_bits,
                "epoch": p.epoch,
                "expected_hashes": p.expected_hashes,
            })),
            "max_provisional": max_provisional,
            "message_quota": quota,
            "listed": listed,
        }),
    }
}

/// The status as JSON.
pub fn status_json(status: &AdmissionStatus) -> serde_json::Value {
    match status {
        AdmissionStatus::Owner => json!({"status": "owner"}),
        AdmissionStatus::Member => json!({"status": "member"}),
        AdmissionStatus::Provisional => json!({"status": "provisional"}),
        AdmissionStatus::Banned => json!({"status": "banned"}),
        AdmissionStatus::NeedsAdmission {
            reason,
            requirements,
        } => json!({
            "status": "needs_admission",
            "reason": reason_text(reason),
            "requirements": requirements_json(requirements),
        }),
    }
}

pub fn reason_text(reason: &NeedsAdmissionReason) -> String {
    match reason {
        NeedsAdmissionReason::NeverAdmitted => "never_admitted".to_string(),
        NeedsAdmissionReason::PolicyChanged(e) => format!("policy_changed: {e}"),
        NeedsAdmissionReason::Pruned => "pruned".to_string(),
        NeedsAdmissionReason::RoomFull => "room_full".to_string(),
        NeedsAdmissionReason::Shadowed => "shadowed".to_string(),
    }
}

/// Search for a nonce on `threads` solvers, one per core by default, reporting
/// the hashes tried about once a second. Returns the nonce and the bits it
/// achieves.
pub fn solve_pow(
    owner_vk: &VerifyingKey,
    member_vk: &VerifyingKey,
    req: &PowRequirement,
    target_bits: u8,
    threads: usize,
    mut progress: impl FnMut(u64, f64),
) -> Result<(u64, u8)> {
    let threads = threads.max(1);
    let stop = Arc::new(AtomicBool::new(false));
    let tried = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::channel();
    const SLICE: u64 = 20_000;
    for i in 0..threads {
        let (stop, tried, tx) = (stop.clone(), tried.clone(), tx.clone());
        let mut solver = PowSolver::new(owner_vk, member_vk, req, target_bits)
            .with_partition(i as u64, threads as u64);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match solver.step(SLICE) {
                    PowStep::Found {
                        nonce,
                        achieved_bits,
                    } => {
                        let _ = tx.send((nonce, achieved_bits));
                        return;
                    }
                    PowStep::Progress { .. } => {
                        tried.fetch_add(SLICE, Ordering::Relaxed);
                    }
                }
            }
        });
    }
    drop(tx);
    let expected = 2f64.powi(target_bits.max(req.difficulty_bits) as i32);
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(found) => {
                stop.store(true, Ordering::Relaxed);
                return Ok(found);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                progress(tried.load(Ordering::Relaxed), expected)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("the proof-of-work solvers stopped without a result")
            }
        }
    }
}

/// Parse a base58 room id (the room owner's verifying key).
pub fn parse_room_id(room_id: &str) -> Result<VerifyingKey> {
    let bytes = bs58::decode(room_id)
        .into_vec()
        .map_err(|e| anyhow!("Invalid room ID: {e}"))?;
    let bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("Invalid room ID length"))?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| anyhow!("Invalid room owner key: {e}"))
}

/// A base64 signing key, or a fresh random one when none is given (and `true`
/// then: the caller must show it, since nothing else stores it).
pub fn key_or_new(signing_key: Option<&str>) -> Result<(SigningKey, bool)> {
    use base64::Engine;
    match signing_key {
        Some(s) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(|e| anyhow!("Invalid signing key (base64 decode failed): {e}"))?;
            let bytes: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| anyhow!("Invalid signing key: expected 32 bytes"))?;
            Ok((SigningKey::from_bytes(&bytes), false))
        }
        None => {
            use rand::RngCore;
            let mut bytes = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut bytes);
            Ok((SigningKey::from_bytes(&bytes), true))
        }
    }
}

/// What `build_join` produced.
pub struct JoinOutcome {
    pub member_id: MemberId,
    pub entry: AuthorizedProvisionalMember,
    pub nonce_bits: Option<u8>,
    pub message_id: river_core::room_state::message::MessageId,
}

/// Most bits `--extra-bits` may add to the room's requirement.
pub const MAX_EXTRA_BITS: u8 = 8;
/// Most proof-of-work threads `--threads` may start.
pub const MAX_THREADS: usize = 256;

/// What `riverctl room join --pow` was asked for.
pub struct JoinOptions<'a> {
    pub nickname: &'a str,
    pub text: &'a str,
    /// Bits to solve beyond the requirement.
    pub extra_bits: u8,
    pub threads: usize,
}

/// Build everything a provisional member needs to join `room_state`'s room as
/// `signing_key`: the entry (the stored one when it still admits, else a fresh
/// proof), the `member_info` and the first message, as one delta.
pub fn build_join(
    storage: &Storage,
    room_state: &ChatRoomStateV1,
    owner_key: &VerifyingKey,
    signing_key: &SigningKey,
    options: &JoinOptions<'_>,
    progress: impl FnMut(u64, f64),
) -> Result<(river_core::room_state::ChatRoomStateV1Delta, JoinOutcome)> {
    if options.extra_bits > MAX_EXTRA_BITS {
        bail!("--extra-bits must be at most {MAX_EXTRA_BITS}");
    }
    if options.threads > MAX_THREADS {
        bail!("--threads must be at most {MAX_THREADS}");
    }
    let params = ChatRoomParametersV1 { owner: *owner_key };
    let member_vk = signing_key.verifying_key();
    let config = &room_state.configuration.configuration;
    let stored = storage.get_provisional_entry(owner_key, &member_vk)?;
    let now = SystemTime::now();
    match admission_status(room_state, &params, &member_vk, stored.as_ref(), now) {
        AdmissionStatus::Owner | AdmissionStatus::Member => {
            bail!("this key is already a member of the room; use `riverctl message send`")
        }
        AdmissionStatus::Banned => bail!("this key is banned from the room"),
        AdmissionStatus::NeedsAdmission {
            reason: NeedsAdmissionReason::Shadowed,
            ..
        } => bail!(
            "another key holds this key's member id in the room; ask the room owner to \
             ban the member who invited it, or use a new key"
        ),
        AdmissionStatus::NeedsAdmission {
            reason: NeedsAdmissionReason::RoomFull,
            ..
        } => bail!(
            "the room's open-member slots are held by newer messages; solving again will \
             not help, try again later"
        ),
        _ => {}
    }
    let req = match admission_requirements(config) {
        AdmissionRequirements::InviteOnly => {
            bail!("this room is invite only; accept an invitation with `riverctl invite accept`")
        }
        AdmissionRequirements::Open { pow: None, .. } => {
            bail!("this room does not admit by proof of work; ask for an invitation")
        }
        AdmissionRequirements::Open { pow: Some(p), .. } => p,
    };
    let held = room_state
        .provisional
        .entries
        .iter()
        .find(|e| e.provisional.member_vk == member_vk)
        .cloned();
    let mut nonce_bits = None;
    let entry = match (held, stored) {
        (Some(e), _) => e,
        (None, Some(e))
            if river_core::room_state::admission::check_admission(room_state, &params, &e)
                .is_ok() =>
        {
            e
        }
        _ => {
            let (nonce, bits) = solve_pow(
                owner_key,
                &member_vk,
                &req,
                req.difficulty_bits
                    .saturating_add(options.extra_bits)
                    .min(MAX_DIFFICULTY_BITS),
                options.threads,
                progress,
            )?;
            nonce_bits = Some(bits);
            let member = river_core::room_state::provisional::ProvisionalMember {
                member_vk,
                evidence: river_core::room_state::provisional::AdmissionEvidence::Pow { nonce },
            };
            let signature = signing_key.sign(&provisional_signing_bytes(owner_key, &member));
            assemble_provisional(member, signature)
        }
    };
    let id = MemberId::from(&member_vk);
    let next_info_version = room_state
        .member_info
        .member_info
        .iter()
        .filter(|i| i.member_info.member_id == id)
        .map(|i| i.member_info.version)
        .max()
        .map_or(1, |v| v + 1);
    let info = AuthorizedMemberInfo::new_with_member_key(
        MemberInfo::new_public(id, next_info_version, options.nickname.to_string()),
        signing_key,
    );
    // Taken after the proof solve, which can run for minutes: a message
    // stamped before it would be older than the work that admits it.
    let message_time = SystemTime::now();
    let message = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: MemberId::from(owner_key),
            author: id,
            content: RoomMessageBody::public(options.text.to_string()),
            time: message_time,
        },
        signing_key,
    );
    let message_id = message.id();
    let delta = admission_delta(room_state, &params, &entry, info, message);
    Ok((
        delta,
        JoinOutcome {
            member_id: id,
            entry,
            nonce_bits,
            message_id,
        },
    ))
}

/// The entry to attach to a send by `signing_key` when the room holds none:
/// `Ok(None)` when the key is a member or the room already holds its entry (or
/// no entry is stored, which `authorize_send` then refuses with the way
/// forward), `Err` when the stored entry cannot be used.
///
/// The stored entry is read only when the key is neither a member nor already
/// held by the room, so an ordinary member send never touches provisional
/// storage.
pub fn attachment_for_send(
    storage: &Storage,
    room_state: &ChatRoomStateV1,
    owner_key: &VerifyingKey,
    signing_key: &SigningKey,
    send_time: SystemTime,
) -> Result<Option<ProvisionalDelta>> {
    let params = ChatRoomParametersV1 { owner: *owner_key };
    let member_vk = signing_key.verifying_key();
    // These outcomes do not depend on a stored entry.
    let status = admission_status(room_state, &params, &member_vk, None, send_time);
    if matches!(
        status,
        AdmissionStatus::Owner | AdmissionStatus::Member | AdmissionStatus::Provisional
    ) {
        return Ok(None);
    }
    let stored = storage.get_provisional_entry(owner_key, &member_vk)?;
    let status = admission_status(room_state, &params, &member_vk, stored.as_ref(), send_time);
    attachment_for_status(status, stored)
}

/// The decision of [`attachment_for_send`] once the status is known.
fn attachment_for_status(
    status: AdmissionStatus,
    stored: Option<AuthorizedProvisionalMember>,
) -> Result<Option<ProvisionalDelta>> {
    match status {
        AdmissionStatus::Owner | AdmissionStatus::Member | AdmissionStatus::Provisional => Ok(None),
        AdmissionStatus::Banned => Err(anyhow!("You are banned from this room")),
        AdmissionStatus::NeedsAdmission { reason, .. } => match (reason, stored) {
            (NeedsAdmissionReason::Pruned, Some(entry)) => Ok(Some(ProvisionalDelta {
                entries: vec![entry],
                bans: vec![],
            })),
            (NeedsAdmissionReason::PolicyChanged(e), Some(_)) => Err(anyhow!(
                "The room's admission policy changed ({e}); run `riverctl room join <room> \
                 --pow` again to solve it"
            )),
            (NeedsAdmissionReason::RoomFull, Some(_)) => Err(anyhow!(
                "The room's open-member slots are held by newer messages; try again later"
            )),
            (NeedsAdmissionReason::Shadowed, _) => Err(anyhow!(
                "Another key holds this key's member id in the room; use a new key"
            )),
            _ => Ok(None),
        },
    }
}

/// Apply `--public-admission` and friends to a configuration copy.
pub struct PolicyEdit {
    pub enable: Option<bool>,
    pub difficulty_bits: Option<u8>,
    pub new_epoch: bool,
    pub max_provisional: Option<u32>,
    pub max_provisional_messages: Option<u32>,
    pub max_provisional_bans: Option<u32>,
    pub listed: Option<bool>,
}

impl PolicyEdit {
    pub fn is_empty(&self) -> bool {
        self.enable.is_none()
            && self.difficulty_bits.is_none()
            && !self.new_epoch
            && self.max_provisional.is_none()
            && self.max_provisional_messages.is_none()
            && self.max_provisional_bans.is_none()
            && self.listed.is_none()
    }

    pub fn apply(&self, config: &mut river_core::room_state::configuration::Configuration) {
        use river_core::room_state::provisional::PowPolicy;
        if self.enable == Some(false) {
            config.public_admission = None;
            return;
        }
        let enabling = self.enable == Some(true);
        if config.public_admission.is_none() {
            if !enabling {
                // Editing a policy the room does not have is a no-op.
                return;
            }
            // Defaults follow the design: the open-member quota is half the
            // message window, and the counts start at their ceilings. The
            // owner may lower them.
            config.public_admission = Some(PublicRoomPolicy {
                listed: false,
                max_provisional: MAX_PROVISIONAL_CEILING,
                max_provisional_messages: (config.max_recent_messages as u32 / 2)
                    .clamp(1, MAX_PROVISIONAL_MESSAGES_CEILING),
                max_provisional_bans: MAX_PROVISIONAL_BANS_CEILING,
                // A random epoch: a fixed one would let proofs for any room be
                // computed in advance, and would revive old proofs when
                // admission is disabled and enabled again.
                pow: Some(PowPolicy {
                    difficulty_bits: 20,
                    epoch: random_epoch(),
                }),
            });
        }
        let policy = config.public_admission.as_mut().expect("set above");
        if let Some(v) = self.max_provisional {
            policy.max_provisional = v;
        }
        if let Some(v) = self.max_provisional_messages {
            policy.max_provisional_messages = v;
        }
        if let Some(v) = self.max_provisional_bans {
            policy.max_provisional_bans = v;
        }
        if let Some(v) = self.listed {
            policy.listed = v;
        }
        if self.difficulty_bits.is_some() || self.new_epoch {
            let held = policy.pow.clone().unwrap_or(PowPolicy {
                difficulty_bits: 20,
                epoch: random_epoch(),
            });
            let mut epoch = held.epoch;
            if self.new_epoch {
                // A fresh random epoch invalidates every proof, stockpiles
                // included.
                epoch = random_epoch();
            }
            policy.pow = Some(PowPolicy {
                difficulty_bits: self.difficulty_bits.unwrap_or(held.difficulty_bits),
                epoch,
            });
        }
    }
}

fn random_epoch() -> u32 {
    use rand::RngCore;
    rand::rngs::OsRng.next_u32()
}

#[cfg(test)]
mod tests {
    use super::*;
    use river_core::room_state::configuration::Configuration;

    /// Whether `config` passes the room's value checks.
    fn adoptable(config: &Configuration) -> bool {
        let params = ChatRoomParametersV1 {
            owner: SigningKey::from_bytes(&[9; 32]).verifying_key(),
        };
        let mut c = config.clone();
        c.owner_member_id = params.owner_id();
        c.check_values(&params).is_ok()
    }

    fn edit() -> PolicyEdit {
        PolicyEdit {
            enable: None,
            difficulty_bits: None,
            new_epoch: false,
            max_provisional: None,
            max_provisional_messages: None,
            max_provisional_bans: None,
            listed: None,
        }
    }

    #[test]
    fn enabling_creates_a_policy_and_disabling_removes_it() {
        let mut c = Configuration::default();
        let mut e = edit();
        e.enable = Some(true);
        e.difficulty_bits = Some(12);
        e.max_provisional = Some(7);
        e.apply(&mut c);
        let p = c.public_admission.clone().unwrap();
        assert_eq!(p.max_provisional, 7);
        assert_eq!(p.pow.unwrap().difficulty_bits, 12);
        assert!(adoptable(&c));
        let mut off = edit();
        off.enable = Some(false);
        off.apply(&mut c);
        assert!(c.public_admission.is_none());
    }

    #[test]
    fn a_new_epoch_changes_only_the_epoch() {
        let mut c = Configuration::default();
        let mut e = edit();
        e.enable = Some(true);
        e.apply(&mut c);
        let before = c.public_admission.clone().unwrap().pow.unwrap();
        let mut again = edit();
        again.new_epoch = true;
        again.apply(&mut c);
        let after = c.public_admission.clone().unwrap().pow.unwrap();
        assert_eq!(before.difficulty_bits, after.difficulty_bits);
        assert_ne!(before.epoch, after.epoch);
    }

    #[test]
    fn editing_without_a_policy_changes_nothing() {
        let mut c = Configuration::default();
        let mut e = edit();
        e.max_provisional = Some(3);
        e.apply(&mut c);
        assert!(c.public_admission.is_none());
    }

    fn open_room(bits: u8) -> (ChatRoomStateV1, ChatRoomParametersV1, SigningKey) {
        use river_core::room_state::configuration::AuthorizedConfigurationV1;
        use river_core::room_state::provisional::PowPolicy;
        let owner = SigningKey::from_bytes(&[200; 32]);
        let params = ChatRoomParametersV1 {
            owner: owner.verifying_key(),
        };
        let config = Configuration {
            owner_member_id: params.owner_id(),
            max_recent_messages: 20,
            public_admission: Some(PublicRoomPolicy {
                listed: false,
                max_provisional: 3,
                max_provisional_messages: 5,
                max_provisional_bans: 3,
                pow: Some(PowPolicy {
                    difficulty_bits: bits,
                    epoch: 2,
                }),
            }),
            ..Configuration::default()
        };
        (
            ChatRoomStateV1 {
                configuration: AuthorizedConfigurationV1::new(config, &owner),
                ..Default::default()
            },
            params,
            owner,
        )
    }

    /// `room join --pow` builds one delta that the room's own apply accepts,
    /// keeps the entry for later sends, and a later send attaches the stored
    /// entry only when the room pruned it.
    #[test]
    fn a_join_builds_a_delta_the_room_accepts_and_a_later_send_reuses_the_entry() {
        use freenet_scaffold::ComposableState;
        let (mut state, params, _owner) = open_room(6);
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(Some(dir.path().to_str().unwrap())).unwrap();
        let k = SigningKey::from_bytes(&[5; 32]);
        let (delta, outcome) = build_join(
            &storage,
            &state,
            &params.owner,
            &k,
            &JoinOptions {
                nickname: "Ann",
                text: "hello",
                extra_bits: 0,
                threads: 2,
            },
            |_, _| {},
        )
        .unwrap();
        storage
            .put_provisional_entry(&params.owner, &k.verifying_key(), &outcome.entry)
            .unwrap();
        let parent = state.clone();
        state.apply_delta(&parent, &params, &Some(delta)).unwrap();
        assert!(state
            .recent_messages
            .messages
            .iter()
            .any(|m| m.id() == outcome.message_id));
        assert_eq!(state.provisional.entries.len(), 1);
        // The room holds the entry: a send attaches nothing.
        let now = SystemTime::now();
        assert!(
            attachment_for_send(&storage, &state, &params.owner, &k, now)
                .unwrap()
                .is_none()
        );
        // Pruned: the stored entry is attached.
        let mut pruned = state.clone();
        pruned.provisional.entries.clear();
        pruned.recent_messages.messages.clear();
        let attach = attachment_for_send(&storage, &pruned, &params.owner, &k, now)
            .unwrap()
            .expect("the stored entry is attached");
        assert_eq!(attach.entries, vec![outcome.entry.clone()]);
        // The stored entry survives a reload.
        let again = Storage::new(Some(dir.path().to_str().unwrap())).unwrap();
        assert_eq!(
            again
                .get_provisional_entry(&params.owner, &k.verifying_key())
                .unwrap(),
            Some(outcome.entry.clone())
        );
        // An invite-only room is refused with the way forward.
        let mut closed = state.clone();
        closed.configuration.configuration.public_admission = None;
        let k2 = SigningKey::from_bytes(&[6; 32]);
        let err = build_join(
            &storage,
            &closed,
            &params.owner,
            &k2,
            &JoinOptions {
                nickname: "Bob",
                text: "hi",
                extra_bits: 0,
                threads: 1,
            },
            |_, _| {},
        )
        .err()
        .expect("invite only")
        .to_string();
        assert!(err.contains("invitation"), "{err}");
    }

    #[test]
    fn enabling_uses_design_defaults() {
        let mut c = Configuration {
            max_recent_messages: 100,
            ..Configuration::default()
        };
        let mut e = edit();
        e.enable = Some(true);
        e.apply(&mut c);
        let p = c.public_admission.unwrap();
        assert_eq!(p.max_provisional_messages, 50);
        assert_eq!(p.max_provisional, MAX_PROVISIONAL_CEILING);
        assert_eq!(p.max_provisional_bans, MAX_PROVISIONAL_BANS_CEILING);
        // Clamped to the ceiling for a large window and to 1 for a tiny one.
        let mut big = Configuration {
            max_recent_messages: 100_000,
            ..Configuration::default()
        };
        e.apply(&mut big);
        assert_eq!(
            big.public_admission.unwrap().max_provisional_messages,
            MAX_PROVISIONAL_MESSAGES_CEILING
        );
        let mut tiny = Configuration {
            max_recent_messages: 1,
            ..Configuration::default()
        };
        e.apply(&mut tiny);
        assert_eq!(tiny.public_admission.unwrap().max_provisional_messages, 1);
    }

    #[test]
    fn re_enabling_picks_a_fresh_epoch_and_other_edits_keep_it() {
        let mut c = Configuration::default();
        let mut on = edit();
        on.enable = Some(true);
        on.apply(&mut c);
        let first = c.public_admission.clone().unwrap().pow.unwrap().epoch;
        let mut other = edit();
        other.max_provisional = Some(4);
        other.difficulty_bits = Some(10);
        other.apply(&mut c);
        assert_eq!(
            c.public_admission.clone().unwrap().pow.unwrap().epoch,
            first
        );
        let mut off = edit();
        off.enable = Some(false);
        off.apply(&mut c);
        on.apply(&mut c);
        let second = c.public_admission.unwrap().pow.unwrap().epoch;
        assert_ne!(second, 1);
        assert_ne!(second, first);
    }

    #[test]
    fn extra_bits_and_threads_are_capped() {
        let (state, params, _owner) = open_room(4);
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(Some(dir.path().to_str().unwrap())).unwrap();
        let k = SigningKey::from_bytes(&[5; 32]);
        for (extra_bits, threads, needle) in [
            (MAX_EXTRA_BITS + 1, 1, "--extra-bits"),
            (0, MAX_THREADS + 1, "--threads"),
        ] {
            let err = build_join(
                &storage,
                &state,
                &params.owner,
                &k,
                &JoinOptions {
                    nickname: "A",
                    text: "t",
                    extra_bits,
                    threads,
                },
                |_, _| {},
            )
            .err()
            .expect("refused")
            .to_string();
            assert!(err.contains(needle), "{err}");
        }
    }

    fn pending_status(reason: NeedsAdmissionReason) -> AdmissionStatus {
        AdmissionStatus::NeedsAdmission {
            reason,
            requirements: AdmissionRequirements::InviteOnly,
        }
    }

    fn some_entry() -> AuthorizedProvisionalMember {
        let (_state, params, _owner) = open_room(1);
        let k = SigningKey::from_bytes(&[5; 32]);
        let member = river_core::room_state::provisional::ProvisionalMember {
            member_vk: k.verifying_key(),
            evidence: river_core::room_state::provisional::AdmissionEvidence::Pow { nonce: 0 },
        };
        let sig = k.sign(&provisional_signing_bytes(&params.owner, &member));
        assemble_provisional(member, sig)
    }

    #[test]
    fn send_attachment_arms() {
        let entry = some_entry();
        let held = Some(entry.clone());
        // Attach only when pruned.
        let a = attachment_for_status(pending_status(NeedsAdmissionReason::Pruned), held.clone())
            .unwrap()
            .unwrap();
        assert_eq!(a.entries, vec![entry.clone()]);
        // Nothing to attach when nothing is stored.
        assert!(
            attachment_for_status(pending_status(NeedsAdmissionReason::Pruned), None)
                .unwrap()
                .is_none()
        );
        assert!(attachment_for_status(
            pending_status(NeedsAdmissionReason::NeverAdmitted),
            held.clone()
        )
        .unwrap()
        .is_none());
        for ok in [
            AdmissionStatus::Owner,
            AdmissionStatus::Member,
            AdmissionStatus::Provisional,
        ] {
            assert!(attachment_for_status(ok, held.clone()).unwrap().is_none());
        }
        let msg = |r: Result<Option<ProvisionalDelta>>| r.expect_err("refused").to_string();
        assert!(
            msg(attachment_for_status(AdmissionStatus::Banned, held.clone())).contains("banned")
        );
        assert!(msg(attachment_for_status(
            pending_status(NeedsAdmissionReason::PolicyChanged(
                river_core::room_state::admission::AdmissionError::BadSignature
            )),
            held.clone()
        ))
        .contains("policy changed"));
        assert!(msg(attachment_for_status(
            pending_status(NeedsAdmissionReason::RoomFull),
            held.clone()
        ))
        .contains("open-member slots"));
        assert!(msg(attachment_for_status(
            pending_status(NeedsAdmissionReason::Shadowed),
            None
        ))
        .contains("member id"));
    }

    /// A member send must not read provisional storage: a corrupt store does
    /// not block it.
    #[test]
    fn a_member_send_does_not_touch_provisional_storage() {
        let (mut state, params, owner) = open_room(4);
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(Some(dir.path().to_str().unwrap())).unwrap();
        std::fs::write(dir.path().join("provisional.json"), b"{ not json").unwrap();
        let k = SigningKey::from_bytes(&[5; 32]);
        assert!(storage
            .get_provisional_entry(&params.owner, &k.verifying_key())
            .is_err());
        let now = SystemTime::now();
        // The owner needs no entry.
        assert!(
            attachment_for_send(&storage, &state, &params.owner, &owner, now)
                .unwrap()
                .is_none()
        );
        // A key the room holds as provisional needs none either.
        state.provisional.entries.push(some_entry());
        assert!(
            attachment_for_send(&storage, &state, &params.owner, &k, now)
                .unwrap()
                .is_none()
        );
        // A key the room has never seen does read the store, and reports it.
        let other = SigningKey::from_bytes(&[6; 32]);
        assert!(attachment_for_send(&storage, &state, &params.owner, &other, now).is_err());
    }

    /// The `AdmissionStatus::Member` early return in `attachment_for_send`: a
    /// key the room holds as a member never reads provisional storage, even
    /// when the stored entry is unreadable (so a member's send cannot fail on
    /// a store it does not need). Without the early return the read errors.
    #[test]
    fn a_member_key_returns_before_reading_provisional_storage() {
        use river_core::room_state::member::{AuthorizedMember, Member};
        let (mut state, params, owner) = open_room(4);
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(Some(dir.path().to_str().unwrap())).unwrap();
        std::fs::write(dir.path().join("provisional.json"), b"{ not json").unwrap();
        let k = SigningKey::from_bytes(&[7; 32]);
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: params.owner_id(),
                invited_by: params.owner_id(),
                member_vk: k.verifying_key(),
            },
            &owner,
        ));
        let now = SystemTime::now();
        assert_eq!(
            admission_status(&state, &params, &k.verifying_key(), None, now),
            AdmissionStatus::Member,
            "premise: the key is a member"
        );
        assert!(storage
            .get_provisional_entry(&params.owner, &k.verifying_key())
            .is_err());
        assert!(
            attachment_for_send(&storage, &state, &params.owner, &k, now)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_solver_threads_find_a_proof() {
        let owner = SigningKey::from_bytes(&[1; 32]).verifying_key();
        let member = SigningKey::from_bytes(&[2; 32]).verifying_key();
        let req = PowRequirement {
            difficulty_bits: 10,
            epoch: 3,
            expected_hashes: 1024.0,
        };
        let (nonce, bits) = solve_pow(&owner, &member, &req, 10, 3, |_, _| {}).unwrap();
        assert!(bits >= 10);
        let h = river_core::room_state::provisional::pow_hash(&owner, &member, 3, nonce);
        assert!(river_core::room_state::provisional::leading_zero_bits(&h) >= 10);
    }
}
