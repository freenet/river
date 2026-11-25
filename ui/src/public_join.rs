//! Joining a public room from inside the read-only view, with proof of work,
//! and posting (freenet/river#975 step 2; the design is #974 section 6).
//!
//! A visitor who presses Join, or types a message and presses Send, gets a
//! fresh member key (stored through the delegate like an invited member's
//! key), solves the room's proof of work in the background, and posts the
//! entry together with its `member_info` and first message in ONE delta
//! (`admission_delta`): a provisional entry is only kept while the room holds a
//! message of its author, so there is no useful entry without a message.
//!
//! # What is where
//!
//! * The pure rules (what the room asks, what a submission is, why it is
//!   refused) are plain functions over `ChatRoomStateV1`, unit-tested natively.
//! * [`JOIN`] is what the view shows: the phase, the draft and the queued
//!   message. It holds nothing secret.
//! * The member key and the solved entry live in a process-memory [`SESSION`],
//!   never in a signal, and never in browser storage.
//! * The message the user sends before admission is the `queued` text. It is
//!   shown as pending, goes out exactly once (inside the admission delta), and
//!   returns to the draft box on any failure or cancel.
//!
//! # Becoming a member
//!
//! On success the room is built from the viewed state plus the submission and
//! inserted into `ROOMS`, which is persisted to the delegate and drives the
//! synchronizer exactly as for any other room: its subscribe GET finds the
//! local state ahead of the network and sends the delta. The viewer then
//! closes. Nothing is written to `ROOMS` before this point.

use crate::public_room::{ViewedRoom, ViewedRoomStatus, VIEWED_ROOM};
use dioxus::prelude::*;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::admission::{
    admission_delta, admission_status, admits_by_pow, assemble_provisional,
    provisional_signing_bytes, AdmissionRequirements, AdmissionStatus, NeedsAdmissionReason,
    PowRequirement, PowSolver, PowStep,
};
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::PrivacyMode;
use river_core::room_state::provisional::{
    AdmissionEvidence, AuthorizedProvisionalMember, ProvisionalMember,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::sync::Mutex;
use std::time::SystemTime;

/// Nonces tried between two yields to the browser. About ten milliseconds of
/// hashing in wasm, so the page stays responsive.
const SLICE: u64 = 10_000;
/// Slices between two progress updates (about a tenth of a second).
const PROGRESS_EVERY: u32 = 10;

/// Why a join stopped, in words the visitor can act on.
#[derive(Debug, Clone, PartialEq)]
pub enum JoinFailure {
    /// The open-member slots are held by newer messages: solving again will not
    /// help.
    RoomFull,
    Banned,
    /// Another key holds this key's member id in the room.
    Shadowed,
    /// The room no longer admits by proof of work (policy removed, or the room
    /// went private).
    NotOpen,
    /// The stored proof no longer passes the room's policy.
    PolicyChanged,
    /// The message is longer than the room allows.
    TooLong(usize),
    /// A re-admission needs the member's own key, and this browser does not
    /// hold it.
    NoKey,
    /// The user's rooms have not finished loading, so it is not yet known
    /// whether they are already in this room.
    NotReady,
    /// The user already holds this room.
    AlreadyMember,
    /// The browser could not hash.
    Internal(String),
}

impl JoinFailure {
    pub fn message(&self) -> String {
        match self {
            Self::RoomFull => "The room's open-member slots are held by newer messages. \
                Try again later, or ask a member for an invitation."
                .to_string(),
            Self::Banned => "You are banned from this room.".to_string(),
            Self::Shadowed => "Another key already holds this identity in the room, and \
                your key stays the same, so trying again will not help. \
                Ask a member for an invitation."
                .to_string(),
            Self::NoKey => "This browser no longer holds your key for this room. \
                Ask a member for an invitation."
                .to_string(),
            Self::NotReady => "Your rooms are still loading. Try again in a moment.".to_string(),
            Self::AlreadyMember => "You are already in this room.".to_string(),
            Self::NotOpen => "This room no longer admits new members this way. \
                Ask a member for an invitation."
                .to_string(),
            Self::PolicyChanged => "The room changed what it asks of newcomers. \
                Join again to meet the new requirement."
                .to_string(),
            Self::TooLong(max) => {
                format!("The message is longer than the room allows ({max} bytes).")
            }
            Self::Internal(e) => format!("Joining failed: {e}"),
        }
    }

    /// Whether pressing Join again can succeed without anything else changing.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::RoomFull | Self::PolicyChanged | Self::NotReady | Self::Internal(_)
        )
    }
}

/// Whether a viewed room offers to join: public, and admitting by proof of
/// work. A private room, or a public one without `public_admission`, has no
/// Join (the read-only notes of #513 stay).
pub fn offers_join(state: &ChatRoomStateV1) -> bool {
    let config = &state.configuration.configuration;
    config.privacy_mode == PrivacyMode::Public && admits_by_pow(config)
}

/// What the room asks of `member_vk` right now, or why it will not take it.
///
/// `entry` is the solved proof, once there is one: only then can the room say
/// whether it would keep the entry (`RoomFull`).
pub fn check_admission_status(
    state: &ChatRoomStateV1,
    owner_vk: &VerifyingKey,
    member_vk: &VerifyingKey,
    entry: Option<&AuthorizedProvisionalMember>,
    send_time: SystemTime,
) -> Result<PowRequirement, JoinFailure> {
    let params = ChatRoomParametersV1 { owner: *owner_vk };
    match admission_status(state, &params, member_vk, entry, send_time) {
        AdmissionStatus::Banned => Err(JoinFailure::Banned),
        AdmissionStatus::Owner | AdmissionStatus::Member | AdmissionStatus::Provisional => {
            // A fresh key is never any of these; the room already holds it.
            Err(JoinFailure::Internal(
                "this key is already in the room".to_string(),
            ))
        }
        AdmissionStatus::NeedsAdmission {
            reason,
            requirements,
        } => {
            match reason {
                NeedsAdmissionReason::RoomFull => return Err(JoinFailure::RoomFull),
                NeedsAdmissionReason::Shadowed => return Err(JoinFailure::Shadowed),
                NeedsAdmissionReason::PolicyChanged(_) => return Err(JoinFailure::PolicyChanged),
                NeedsAdmissionReason::NeverAdmitted | NeedsAdmissionReason::Pruned => {}
            }
            match requirements {
                AdmissionRequirements::Open { pow: Some(p), .. } => Ok(p),
                _ => Err(JoinFailure::NotOpen),
            }
        }
    }
}

/// Everything a successful join produces.
#[derive(Debug, Clone)]
pub struct Submission {
    pub delta: ChatRoomStateV1Delta,
    /// The room as the visitor holds it once the delta is applied.
    pub state_after: ChatRoomStateV1,
    pub info: AuthorizedMemberInfo,
    pub nickname: String,
    /// The member the delta admits and the messages it carries, so a later
    /// apply to a newer state can confirm the room kept all of them.
    pub member_vk: VerifyingKey,
    pub message_ids: Vec<river_core::room_state::message::MessageId>,
}

/// [`build_submission_many`] for one message.
pub fn build_submission(
    state: &ChatRoomStateV1,
    owner_vk: &VerifyingKey,
    signing_key: &SigningKey,
    entry: &AuthorizedProvisionalMember,
    text: &str,
    nickname: &str,
    now: SystemTime,
) -> Result<Submission, JoinFailure> {
    build_submission_many(
        state,
        owner_vk,
        signing_key,
        entry,
        &[text.to_string()],
        nickname,
        now,
    )
}

/// Build the one delta that admits `signing_key` with `texts` as its first
/// messages (in order, a millisecond apart), and apply it to a copy of `state`.
///
/// Refuses, with the reason, when the room would not keep the entry or every
/// message: the status is asked of the room's own cleanup, and the result is
/// checked to hold all of them.
pub fn build_submission_many(
    state: &ChatRoomStateV1,
    owner_vk: &VerifyingKey,
    signing_key: &SigningKey,
    entry: &AuthorizedProvisionalMember,
    texts: &[String],
    nickname: &str,
    now: SystemTime,
) -> Result<Submission, JoinFailure> {
    let config = &state.configuration.configuration;
    if texts.is_empty() {
        return Err(JoinFailure::Internal("no message to post".to_string()));
    }
    if texts.iter().any(|t| t.len() > config.max_message_size) {
        return Err(JoinFailure::TooLong(config.max_message_size));
    }
    let member_vk = signing_key.verifying_key();
    check_admission_status(state, owner_vk, &member_vk, Some(entry), now)?;
    let params = ChatRoomParametersV1 { owner: *owner_vk };
    let id = MemberId::from(&member_vk);
    let next_version = state
        .member_info
        .member_info
        .iter()
        .filter(|i| i.member_info.member_id == id)
        .map(|i| i.member_info.version)
        .max()
        .map_or(1, |v| v + 1);
    let info = AuthorizedMemberInfo::new_with_member_key(
        MemberInfo::new_public(id, next_version, nickname.to_string()),
        signing_key,
    );
    let messages: Vec<AuthorizedMessageV1> = texts
        .iter()
        .enumerate()
        .map(|(i, text)| {
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: MemberId::from(owner_vk),
                    author: id,
                    content: RoomMessageBody::public(text.clone()),
                    time: (now + std::time::Duration::from_millis(i as u64)).into(),
                },
                signing_key,
            )
        })
        .collect();
    let message_ids: Vec<_> = messages.iter().map(|m| m.id()).collect();
    let mut messages = messages.into_iter();
    let first = messages.next().expect("texts is not empty");
    let mut delta = admission_delta(state, &params, entry, info.clone(), first);
    if let Some(list) = delta.recent_messages.as_mut() {
        list.extend(messages);
    }
    let mut state_after = state.clone();
    state_after
        .apply_delta(state, &params, &Some(delta.clone()))
        .map_err(|e| JoinFailure::Internal(format!("{e:?}")))?;
    let kept = message_ids.iter().all(|id| {
        state_after
            .recent_messages
            .messages
            .iter()
            .any(|m| m.id() == *id)
    }) && state_after
        .provisional
        .entries
        .iter()
        .any(|e| e.provisional.member_vk == member_vk);
    if !kept {
        // The room's cleanup dropped the entry or a message: the pool or the
        // quota is held by newer messages.
        return Err(JoinFailure::RoomFull);
    }
    state_after.recent_messages.rebuild_actions_state();
    Ok(Submission {
        delta,
        state_after,
        info,
        nickname: nickname.to_string(),
        member_vk,
        message_ids,
    })
}

/// A proof for `nonce`, signed by the member's own key.
pub fn entry_for_nonce(
    owner_vk: &VerifyingKey,
    signing_key: &SigningKey,
    nonce: u64,
) -> AuthorizedProvisionalMember {
    let member = ProvisionalMember {
        member_vk: signing_key.verifying_key(),
        evidence: AdmissionEvidence::Pow { nonce },
    };
    let signature = signing_key.sign(&provisional_signing_bytes(owner_vk, &member));
    assemble_provisional(member, signature)
}

/// The visitor's draft and `queued` message come back together: a queued
/// message returns to the draft box in front of whatever was typed since.
pub fn restore_draft(draft: &str, queued: Option<&str>) -> String {
    match queued {
        None => draft.to_string(),
        Some(q) if draft.trim().is_empty() => q.to_string(),
        Some(q) => format!("{q}\n{draft}"),
    }
}

/// What the join is doing, for the view.
#[derive(Debug, Clone, PartialEq)]
pub enum JoinPhase {
    /// Not started.
    Idle,
    /// Solving the proof of work.
    Solving {
        tried: u64,
        expected: f64,
    },
    /// Admitted-ready: the proof is held, and the next message posts it.
    Ready,
    /// Posting.
    Submitting,
    Failed(JoinFailure),
}

/// What the view shows. Nothing secret. `owner` says which room the state is
/// about: a composer for any other room shows an idle join.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinUi {
    pub owner: Option<VerifyingKey>,
    pub phase: JoinPhase,
    /// Messages sent before admission, in order: shown as pending, posted once.
    pub queued: Vec<String>,
    /// Text handed back after a failure, cancel or close, per room. The
    /// composer of that room puts it back in its draft box and takes it.
    pub returned: Vec<(VerifyingKey, String)>,
    /// A refusal that does not stop the join in progress (a message that is
    /// too long, or one sent while posting): shown with the composer.
    pub notice: Option<String>,
}

impl Default for JoinUi {
    fn default() -> Self {
        Self {
            owner: None,
            phase: JoinPhase::Idle,
            queued: Vec::new(),
            returned: Vec::new(),
            notice: None,
        }
    }
}

impl JoinUi {
    /// The phase as `owner`'s composer shows it.
    pub fn phase_for(&self, owner: &VerifyingKey) -> JoinPhase {
        if self.owner.as_ref() == Some(owner) {
            self.phase.clone()
        } else {
            JoinPhase::Idle
        }
    }

    /// The pending messages of `owner`'s room.
    pub fn queued_for(&self, owner: &VerifyingKey) -> Vec<String> {
        if self.owner.as_ref() == Some(owner) {
            self.queued.clone()
        } else {
            Vec::new()
        }
    }

    pub fn notice_for(&self, owner: &VerifyingKey) -> Option<String> {
        if self.owner.as_ref() == Some(owner) {
            self.notice.clone()
        } else {
            None
        }
    }

    pub fn has_returned_for(&self, owner: &VerifyingKey) -> bool {
        self.returned.iter().any(|(o, _)| o == owner)
    }
}

pub static JOIN: GlobalSignal<JoinUi> = Global::new(JoinUi::default);

/// What the next step is, once a transition has been recorded. Returned by the
/// [`Machine`] so the driver does nothing but carry it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Solve the proof of work for this generation.
    Solve(u64),
    /// Post the in-flight messages for this generation.
    Submit(u64),
}

/// What a join starts from, worked out by the caller from `ROOMS`.
pub struct StartPlan {
    /// Re-admitting a member already in `ROOMS` (pruned), not joining from the
    /// viewer: the key is the room's own.
    pub readmit: bool,
    /// The key to join with: fresh for a join, the room's own for a re-admission
    /// (`None` when this browser does not hold it).
    pub key: Option<SigningKey>,
    /// The proof held from earlier, if the room's own member has one.
    pub entry: Option<AuthorizedProvisionalMember>,
    /// A reason not to start at all (rooms still loading, already a member).
    pub blocker: Option<JoinFailure>,
}

/// What the room allows a message, as far as the queue is concerned.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_message_size: usize,
    /// How many messages the entry may carry (the room's provisional message
    /// quota). More than this would be dropped by the room's own cleanup.
    pub max_queued: usize,
}

/// The state of the join in progress, and every transition it can make. Pure:
/// no signals, no browser, so each rule is tested natively and the driver
/// below only carries out the [`Action`] a transition returns.
///
/// Rules this type owns:
/// * a message is in exactly one of: the queue, the in-flight batch, the
///   returned text. It leaves the in-flight batch only by `succeed`.
/// * the phase flips to `Submitting` in the same step that takes the queue, so
///   two sends cannot both submit.
/// * a message is never dropped: every refusal and failure hands it back.
pub struct Machine {
    generation: u64,
    owner: Option<VerifyingKey>,
    phase: JoinPhase,
    queue: Vec<String>,
    /// The batch being posted. Not in `queue`, not yet in the room.
    in_flight: Vec<String>,
    returned: Vec<(VerifyingKey, String)>,
    notice: Option<String>,
    signing_key: Option<SigningKey>,
    entry: Option<AuthorizedProvisionalMember>,
    readmit: bool,
    /// The room a join just completed for: lets its composer's leftover draft
    /// follow the user into the member room.
    joined: Option<VerifyingKey>,
    /// The text in a composer's draft box, kept as it is typed (not read when
    /// the composer closes: the member room's composer may be built before the
    /// join composer is dropped).
    draft: Option<(VerifyingKey, String)>,
}

impl Machine {
    pub const fn new() -> Self {
        Self {
            generation: 0,
            owner: None,
            phase: JoinPhase::Idle,
            queue: Vec::new(),
            in_flight: Vec::new(),
            returned: Vec::new(),
            notice: None,
            signing_key: None,
            entry: None,
            readmit: false,
            joined: None,
            draft: None,
        }
    }

    /// What the view shows.
    pub fn ui(&self) -> JoinUi {
        JoinUi {
            owner: self.owner,
            phase: self.phase.clone(),
            queued: self.in_flight.iter().chain(&self.queue).cloned().collect(),
            returned: self.returned.clone(),
            notice: self.notice.clone(),
        }
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    fn active(&self) -> bool {
        matches!(
            self.phase,
            JoinPhase::Solving { .. } | JoinPhase::Ready | JoinPhase::Submitting
        )
    }

    /// Put `text` back for `owner`'s composer, after anything already waiting.
    fn give_back(&mut self, owner: VerifyingKey, text: String) {
        match self.returned.iter_mut().find(|(o, _)| *o == owner) {
            Some((_, t)) => {
                if t.trim().is_empty() {
                    *t = text;
                } else {
                    t.push('\n');
                    t.push_str(&text);
                }
            }
            None => self.returned.push((owner, text)),
        }
    }

    /// Hand every message not yet in the room back, in the order sent.
    fn hand_back_all(&mut self) {
        let Some(owner) = self.owner else {
            self.queue.clear();
            self.in_flight.clear();
            return;
        };
        let mut all = std::mem::take(&mut self.in_flight);
        all.append(&mut self.queue);
        if !all.is_empty() {
            self.give_back(owner, all.join("\n"));
        }
    }

    fn clear_secrets(&mut self) {
        self.signing_key = None;
        self.entry = None;
        self.readmit = false;
    }

    /// Stop whatever is under way for another room. Its messages are handed
    /// back to that room's composer.
    fn abandon(&mut self) {
        self.generation += 1;
        self.hand_back_all();
        self.clear_secrets();
        self.phase = JoinPhase::Idle;
        self.notice = None;
    }

    fn begin(&mut self, owner: VerifyingKey, plan: StartPlan) -> Action {
        if let Some(blocker) = plan.blocker {
            self.generation += 1;
            self.owner = Some(owner);
            self.hand_back_all();
            self.clear_secrets();
            self.phase = JoinPhase::Failed(blocker);
            return Action::Nothing;
        }
        let Some(key) = plan.key else {
            self.generation += 1;
            self.owner = Some(owner);
            self.hand_back_all();
            self.clear_secrets();
            self.phase = JoinPhase::Failed(JoinFailure::NoKey);
            return Action::Nothing;
        };
        self.generation += 1;
        self.owner = Some(owner);
        self.signing_key = Some(key);
        self.entry = plan.entry;
        self.readmit = plan.readmit;
        self.joined = None;
        self.notice = None;
        self.phase = JoinPhase::Solving {
            tried: 0,
            expected: 0.0,
        };
        Action::Solve(self.generation)
    }

    /// Whether a join for `owner` can start from here.
    fn can_start(&self, owner: &VerifyingKey) -> bool {
        match (&self.owner, &self.phase) {
            (_, JoinPhase::Submitting) => false,
            (Some(o), p) if o == owner => matches!(p, JoinPhase::Idle | JoinPhase::Failed(_)),
            // Another room's join: only an unfinished one that is not posting
            // is dropped for this one.
            _ => true,
        }
    }

    /// Join pressed. Nothing happens while one is under way for this room.
    pub fn start(&mut self, owner: VerifyingKey, plan: StartPlan) -> Action {
        if !self.can_start(&owner) {
            return Action::Nothing;
        }
        if self.owner.is_some_and(|o| o != owner) && self.active() {
            self.abandon();
        }
        self.begin(owner, plan)
    }

    /// Send pressed with `text`: queue it, start admission or post it.
    pub fn send(
        &mut self,
        owner: VerifyingKey,
        text: &str,
        limits: Limits,
        plan: StartPlan,
    ) -> Action {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Action::Nothing;
        }
        if text.len() > limits.max_message_size {
            // Before any work is done: a valid proof would be wasted on it.
            self.give_back(owner, text);
            if self.owner != Some(owner) && !self.active() {
                // Another room's leftover state must not show in this room.
                self.owner = Some(owner);
                self.phase = JoinPhase::Idle;
                self.clear_secrets();
            }
            self.notice = Some(JoinFailure::TooLong(limits.max_message_size).message());
            return Action::Nothing;
        }
        let mine = self.owner == Some(owner);
        if matches!(self.phase, JoinPhase::Submitting) {
            // The text goes back to the box it came from; the posting batch is
            // not touched.
            self.give_back(owner, text);
            if mine {
                self.notice =
                    Some("Still posting your last message. Send again in a moment.".into());
            }
            return Action::Nothing;
        }
        if self.owner.is_some_and(|o| o != owner) && self.active() {
            self.abandon();
        }
        let mine = self.owner == Some(owner);
        if mine && matches!(self.phase, JoinPhase::Solving { .. } | JoinPhase::Ready) {
            if self.queue.len() + self.in_flight.len() >= limits.max_queued {
                self.give_back(owner, text);
                self.notice = Some(format!(
                    "You can queue {} messages before you are admitted. Wait for admission to send more.",
                    limits.max_queued
                ));
                return Action::Nothing;
            }
            self.notice = None;
            self.queue.push(text);
            if matches!(self.phase, JoinPhase::Ready) {
                self.in_flight = std::mem::take(&mut self.queue);
                self.phase = JoinPhase::Submitting;
                return Action::Submit(self.generation);
            }
            return Action::Nothing;
        }
        // Idle or failed (or another room's finished state): start.
        self.owner = Some(owner);
        self.queue.push(text);
        self.begin(owner, plan)
    }

    pub fn progress(&mut self, generation: u64, tried: u64, expected: f64) {
        if self.is_current(generation) && matches!(self.phase, JoinPhase::Solving { .. }) {
            self.phase = JoinPhase::Solving { tried, expected };
        }
    }

    /// The proof is solved (or a held one still passes). Posts the queue, or
    /// waits for the first message.
    pub fn solved(&mut self, generation: u64, entry: AuthorizedProvisionalMember) -> Action {
        if !self.is_current(generation) || !matches!(self.phase, JoinPhase::Solving { .. }) {
            return Action::Nothing;
        }
        self.entry = Some(entry);
        if self.queue.is_empty() {
            self.phase = JoinPhase::Ready;
            Action::Nothing
        } else {
            self.in_flight = std::mem::take(&mut self.queue);
            self.phase = JoinPhase::Submitting;
            Action::Submit(generation)
        }
    }

    /// The proof stopped passing before it could be posted: solve again with
    /// the same key, keeping every message.
    pub fn restart_solving(&mut self, generation: u64) -> Action {
        if !self.is_current(generation) {
            return Action::Nothing;
        }
        let mut all = std::mem::take(&mut self.in_flight);
        all.append(&mut self.queue);
        self.queue = all;
        self.entry = None;
        self.generation += 1;
        self.phase = JoinPhase::Solving {
            tried: 0,
            expected: 0.0,
        };
        Action::Solve(self.generation)
    }

    pub fn fail(&mut self, generation: u64, failure: JoinFailure) {
        if !self.is_current(generation) {
            return;
        }
        self.hand_back_all();
        self.clear_secrets();
        self.phase = JoinPhase::Failed(failure);
    }

    /// Cancel solving. Every pending message returns to the composer, and the
    /// key made for it is forgotten.
    pub fn cancel(&mut self) {
        self.generation += 1;
        self.hand_back_all();
        self.clear_secrets();
        self.notice = None;
        self.phase = JoinPhase::Idle;
    }

    /// The viewer closed or the room changed: as cancel, and no room is
    /// remembered.
    pub fn reset(&mut self) {
        self.cancel();
        self.owner = None;
    }

    /// The batch is in the room. Returns the messages that were posted.
    pub fn succeed(&mut self, generation: u64) -> Option<Vec<String>> {
        if !self.is_current(generation) || !matches!(self.phase, JoinPhase::Submitting) {
            return None;
        }
        let posted = std::mem::take(&mut self.in_flight);
        self.joined = self.owner;
        self.clear_secrets();
        self.owner = None;
        self.notice = None;
        self.phase = JoinPhase::Idle;
        Some(posted)
    }

    pub fn take_returned(&mut self, owner: &VerifyingKey) -> Option<String> {
        let i = self.returned.iter().position(|(o, _)| o == owner)?;
        Some(self.returned.remove(i).1)
    }

    /// Record the text now in `owner`'s draft box.
    pub fn note_draft(&mut self, owner: VerifyingKey, text: &str) {
        if text.trim().is_empty() {
            if self.draft.as_ref().is_some_and(|(o, _)| *o == owner) {
                self.draft = None;
            }
        } else {
            self.draft = Some((owner, text.to_string()));
        }
    }

    /// The draft a composer left for `owner`'s member room, once, after a
    /// completed join or re-admission.
    pub fn take_carried(&mut self, owner: &VerifyingKey) -> Option<String> {
        if self.joined.as_ref() != Some(owner) {
            return None;
        }
        self.joined = None;
        match self.draft.take() {
            Some((o, text)) if o == *owner => Some(text),
            other => {
                self.draft = other;
                None
            }
        }
    }

    fn submit_inputs(
        &self,
        generation: u64,
    ) -> Option<(SigningKey, AuthorizedProvisionalMember, Vec<String>, bool)> {
        if !self.is_current(generation) || !matches!(self.phase, JoinPhase::Submitting) {
            return None;
        }
        Some((
            self.signing_key.clone()?,
            self.entry.clone()?,
            self.in_flight.clone(),
            self.readmit,
        ))
    }
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

static MACHINE: Mutex<Machine> = Mutex::new(Machine::new());

fn machine() -> std::sync::MutexGuard<'static, Machine> {
    MACHINE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run `f` on the machine, show the result, and carry out what it returns.
fn step<T>(f: impl FnOnce(&mut Machine) -> T) -> T {
    let (out, ui) = {
        let mut m = machine();
        let out = f(&mut m);
        (out, m.ui())
    };
    update_ui(move |j| *j = ui);
    out
}

fn act(action: Action, owner_vk: VerifyingKey) {
    match action {
        Action::Nothing => {}
        Action::Solve(generation) => {
            crate::util::safe_spawn_local(async move { run(generation, owner_vk).await });
        }
        Action::Submit(generation) => {
            crate::util::safe_spawn_local(async move { submit(generation, owner_vk).await });
        }
    }
}

fn is_current(generation: u64) -> bool {
    machine().is_current(generation)
}

fn fail(generation: u64, failure: JoinFailure) {
    step(|m| m.fail(generation, failure));
}

/// The proof each room last admitted this session, so a member the room pruned
/// is re-submitted at once when the policy still accepts it. Process memory
/// only: after a reload the proof is solved again, with the same key.
static HELD: Mutex<Vec<(VerifyingKey, AuthorizedProvisionalMember)>> = Mutex::new(Vec::new());

fn held_lock() -> std::sync::MutexGuard<'static, Vec<(VerifyingKey, AuthorizedProvisionalMember)>> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

fn remember_entry(owner_vk: VerifyingKey, entry: AuthorizedProvisionalMember) {
    let mut held = held_lock();
    held.retain(|(o, _)| *o != owner_vk);
    held.push((owner_vk, entry));
}

/// The proof held for `owner_vk`'s room, if this session made one.
pub fn held_entry(owner_vk: &VerifyingKey) -> Option<AuthorizedProvisionalMember> {
    held_lock()
        .iter()
        .find(|(o, _)| o == owner_vk)
        .map(|(_, e)| e.clone())
}

/// Test hook support: forget every held proof, as a reload does.
#[cfg(all(target_arch = "wasm32", feature = "example-data", feature = "no-sync"))]
pub fn forget_held_entries() {
    held_lock().clear();
}

/// The room state the join works against: the viewed room, or for a
/// re-admission the member's own room.
fn source_state(owner_vk: &VerifyingKey, readmit: bool) -> Option<ChatRoomStateV1> {
    // Called from the solver task, which does not start inside the runtime.
    crate::util::in_runtime(|| {
        if readmit {
            let rooms = crate::components::app::ROOMS.try_peek().ok()?;
            return rooms.map.get(owner_vk).map(|r| r.room_state.clone());
        }
        viewed_state().and_then(|(o, s)| (o == *owner_vk).then_some(s))
    })
}

/// What to tell a member the room pruned, and what their next message does.
/// `entry` is the proof held for the room, if any.
pub fn readmit_note(
    state: &ChatRoomStateV1,
    owner_vk: &VerifyingKey,
    member_vk: &VerifyingKey,
    entry: Option<&AuthorizedProvisionalMember>,
    now: SystemTime,
) -> String {
    match check_admission_status(state, owner_vk, member_vk, entry, now) {
        Ok(_) if entry.is_some() => {
            "The room removed you for being quiet. Your next message rejoins.".to_string()
        }
        Ok(_) | Err(JoinFailure::PolicyChanged) => "The room removed you for being quiet. \
            Your next message rejoins after a short proof of work."
            .to_string(),
        Err(f) => f.message(),
    }
}

/// [`readmit_note`] for a room the user holds.
pub fn readmit_note_for(room: &crate::room_data::RoomData) -> Option<String> {
    let member_vk = room.self_verifying_key()?;
    Some(readmit_note(
        &room.room_state,
        &room.owner_vk,
        &member_vk,
        held_entry(&room.owner_vk).as_ref(),
        crate::util::get_current_system_time(),
    ))
}

/// Write the UI state from a clean execution context.
fn update_ui(f: impl FnOnce(&mut JoinUi) + 'static) {
    crate::util::defer(move || {
        JOIN.with_mut(f);
    });
}

/// The viewed room's state and owner key, if a public room is being viewed.
fn viewed_state() -> Option<(VerifyingKey, ChatRoomStateV1)> {
    let viewed = VIEWED_ROOM.try_peek().ok()?;
    match viewed.as_ref()? {
        ViewedRoom {
            owner_vk: Some(owner),
            status: ViewedRoomStatus::Public(state),
        } => Some((*owner, (**state).clone())),
        _ => None,
    }
}

/// The verifying key of the join in progress (test hooks).
#[cfg(all(target_arch = "wasm32", feature = "example-data", feature = "no-sync"))]
pub fn session_member_vk() -> Option<VerifyingKey> {
    machine().signing_key.as_ref().map(|k| k.verifying_key())
}

/// Test hook support: while set, the solver does no work, so a test that must
/// cancel or ban mid-solve does not race a lucky proof.
#[cfg(all(target_arch = "wasm32", feature = "example-data", feature = "no-sync"))]
pub static HOLD_SOLVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Forget the join: it is over, or the viewer closed. Messages not yet in the
/// room go back to their composer.
pub fn reset() {
    step(|m| m.reset());
}

/// Cancel solving. The queued messages return to the draft box.
pub fn cancel() {
    step(|m| m.cancel());
}

/// A room's own messages and rooms loading are both needed before a join can
/// be started: a key must never be made for a room the user already holds.
fn start_blocker(owner_vk: &VerifyingKey) -> Option<JoinFailure> {
    crate::util::in_runtime(|| start_blocker_in_runtime(owner_vk))
}

fn start_blocker_in_runtime(owner_vk: &VerifyingKey) -> Option<JoinFailure> {
    use crate::components::app::chat_delegate::{RoomsLoadState, ROOMS_LOAD_STATE};
    // The example build has no delegate to load from: its rooms are all there
    // is, and the load state never leaves `Loading`.
    let loaded = cfg!(feature = "no-sync")
        || ROOMS_LOAD_STATE
            .try_peek()
            .map(|s| *s == RoomsLoadState::Loaded)
            .unwrap_or(false);
    if !loaded {
        return Some(JoinFailure::NotReady);
    }
    match crate::components::app::ROOMS.try_peek() {
        Ok(rooms) if rooms.map.contains_key(owner_vk) => Some(JoinFailure::AlreadyMember),
        Ok(_) => None,
        Err(_) => Some(JoinFailure::NotReady),
    }
}

/// A fresh member key for a join from the viewer. The key is only stored once
/// the entry is posted (`submit`), so an abandoned attempt leaves nothing.
fn fresh_plan(owner_vk: &VerifyingKey) -> StartPlan {
    StartPlan {
        readmit: false,
        key: Some(SigningKey::generate(&mut rand::thread_rng())),
        entry: None,
        blocker: start_blocker(owner_vk),
    }
}

/// The member's own key and held proof, for a room the room pruned them from.
fn readmit_plan(owner_vk: &VerifyingKey) -> StartPlan {
    StartPlan {
        readmit: true,
        key: crate::components::app::ROOMS
            .try_peek()
            .ok()
            .and_then(|r| r.map.get(owner_vk).and_then(|d| d.signing_key().cloned())),
        entry: held_entry(owner_vk),
        blocker: None,
    }
}

fn plan_for(owner_vk: &VerifyingKey) -> (StartPlan, Option<ChatRoomStateV1>) {
    match viewed_state() {
        Some((o, state)) if o == *owner_vk => (fresh_plan(owner_vk), Some(state)),
        _ => {
            let state = crate::components::app::ROOMS
                .try_peek()
                .ok()
                .and_then(|r| r.map.get(owner_vk).map(|d| d.room_state.clone()));
            (readmit_plan(owner_vk), state)
        }
    }
}

/// Start admission (Join pressed). A no-op while one is under way.
pub fn start(owner_vk: VerifyingKey) {
    let action = step(|m| m.start(owner_vk, fresh_plan(&owner_vk)));
    act(action, owner_vk);
}

/// Re-admit the member of a room the room pruned, with the SAME key. A no-op
/// while one is under way.
pub fn start_readmit(owner_vk: VerifyingKey) {
    let action = step(|m| m.start(owner_vk, readmit_plan(&owner_vk)));
    act(action, owner_vk);
}

/// Send pressed with `text`: queue it, and start admission or post it.
pub fn send(owner_vk: VerifyingKey, text: String) {
    let (plan, state) = plan_for(&owner_vk);
    let limits = match &state {
        Some(s) => Limits {
            max_message_size: s.configuration.configuration.max_message_size,
            max_queued: s
                .configuration
                .configuration
                .public_admission
                .as_ref()
                .map_or(1, |p| (p.max_provisional_messages as usize).max(1)),
        },
        None => Limits {
            max_message_size: usize::MAX,
            max_queued: 1,
        },
    };
    let action = step(|m| m.send(owner_vk, &text, limits, plan));
    act(action, owner_vk);
}

/// Take the text handed back to `owner`'s composer, if any.
pub fn take_returned(owner_vk: &VerifyingKey) -> Option<String> {
    let taken = machine().take_returned(owner_vk);
    if taken.is_some() {
        let ui = machine().ui();
        update_ui(move |j| *j = ui);
    }
    taken
}

/// Record the text in `owner`'s draft box, so it can follow the user into the
/// member room after a join.
pub fn note_draft(owner_vk: VerifyingKey, text: &str) {
    machine().note_draft(owner_vk, text);
}

/// The draft the viewer's composer left for the room now selected, for the
/// member room's composer to start with.
pub fn take_carried_draft() -> String {
    let Some(owner) = crate::components::app::CURRENT_ROOM
        .try_peek()
        .ok()
        .and_then(|c| c.owner_key)
    else {
        return String::new();
    };
    let taken = machine().take_carried(&owner);
    if taken.is_some() {
        let ui = machine().ui();
        update_ui(move |j| *j = ui);
    }
    taken.unwrap_or_default()
}

async fn run(generation: u64, owner_vk: VerifyingKey) {
    let (signing_key, readmit, held) = {
        let m = machine();
        if !m.is_current(generation) {
            return;
        }
        let Some(k) = m.signing_key.clone() else {
            return;
        };
        (k, m.readmit, m.entry.clone())
    };
    let member_vk = signing_key.verifying_key();

    let Some(state) = source_state(&owner_vk, readmit) else {
        fail(generation, JoinFailure::NotOpen);
        return;
    };
    // A proof held from earlier is re-submitted when the room's current policy
    // still accepts it; otherwise it is solved again with the same key.
    if let Some(entry) = held {
        let now = crate::util::get_current_system_time();
        match check_admission_status(&state, &owner_vk, &member_vk, Some(&entry), now) {
            Ok(_) => {
                let action = step(|m| m.solved(generation, entry));
                return act(action, owner_vk);
            }
            Err(JoinFailure::PolicyChanged) => {}
            Err(f) => return fail(generation, f),
        }
    }
    let mut req = match check_admission_status(
        &state,
        &owner_vk,
        &member_vk,
        None,
        crate::util::get_current_system_time(),
    ) {
        Ok(r) => r,
        Err(f) => return fail(generation, f),
    };
    let mut solver = PowSolver::new(&owner_vk, &member_vk, &req, req.difficulty_bits);
    let mut slices = 0u32;
    let nonce = loop {
        if !is_current(generation) {
            return;
        }
        // Test hook: a held solver skips the hashing but still re-checks the
        // room below, so a ban or policy change is seen.
        #[cfg(all(target_arch = "wasm32", feature = "example-data", feature = "no-sync"))]
        let held_for_test = HOLD_SOLVER.load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(all(target_arch = "wasm32", feature = "example-data", feature = "no-sync")))]
        let held_for_test = false;
        if !held_for_test {
            match solver.step(SLICE) {
                PowStep::Found { nonce, .. } => break nonce,
                PowStep::Progress { tried, expected } => {
                    slices += 1;
                    if slices % PROGRESS_EVERY == 1 {
                        step(|m| m.progress(generation, tried, expected));
                    }
                }
            }
        }
        // The room may change what it asks of newcomers while this runs
        // (a new epoch restarts the search, more bits raise the target); one
        // that no longer admits stops it.
        if let Some(now_state) = source_state(&owner_vk, readmit) {
            match check_admission_status(
                &now_state,
                &owner_vk,
                &member_vk,
                None,
                crate::util::get_current_system_time(),
            ) {
                Ok(new_req) if new_req != req => {
                    solver.retarget(&new_req, new_req.difficulty_bits);
                    req = new_req;
                }
                Ok(_) => {}
                Err(f) => return fail(generation, f),
            }
        }
        // Yield to the browser so the page stays responsive and Cancel works.
        crate::util::sleep(std::time::Duration::from_millis(0)).await;
    };
    if !is_current(generation) {
        return;
    }
    let entry = entry_for_nonce(&owner_vk, &signing_key, nonce);
    let action = step(|m| m.solved(generation, entry));
    act(action, owner_vk);
}

/// The nickname a join or re-admission publishes: a re-admitted member keeps
/// the one they have.
fn nickname_for(owner_vk: &VerifyingKey, signing_key: &SigningKey, readmit: bool) -> String {
    if readmit {
        let kept = crate::util::in_runtime(|| {
            crate::components::app::ROOMS
                .try_peek()
                .ok()
                .and_then(|r| r.map.get(owner_vk).and_then(|d| d.self_nickname.clone()))
        });
        if let Some(n) = kept {
            return n;
        }
    }
    crate::nickname::generate_default_nickname(&signing_key.verifying_key())
}

/// Post the entry with the in-flight messages, and make the room the
/// visitor's own.
async fn submit(generation: u64, owner_vk: VerifyingKey) {
    let Some((signing_key, entry, texts, readmit)) = machine().submit_inputs(generation) else {
        return;
    };
    let Some(state) = source_state(&owner_vk, readmit) else {
        return fail(generation, JoinFailure::NotOpen);
    };
    let nickname = nickname_for(&owner_vk, &signing_key, readmit);
    let submission = match build_submission_many(
        &state,
        &owner_vk,
        &signing_key,
        &entry,
        &texts,
        &nickname,
        crate::util::get_current_system_time(),
    ) {
        Ok(s) => s,
        Err(JoinFailure::PolicyChanged) => {
            // The proof no longer passes: solve again, keeping the messages,
            // instead of dropping the visitor into an error.
            let action = step(|m| m.restart_solving(generation));
            return act(action, owner_vk);
        }
        Err(f) => return fail(generation, f),
    };

    if !is_current(generation) {
        return;
    }
    // Rooms must be loaded and not hold this room before the room is inserted.
    // Nothing is stored in the delegate yet: a close or cancel up to here
    // leaves no key behind.
    crate::util::defer(move || {
        if !is_current(generation) {
            return;
        }
        if !readmit {
            if let Some(blocker) = start_blocker(&owner_vk) {
                return fail(generation, blocker);
            }
        }
        let stored_key = signing_key.clone();
        let applied = if readmit {
            apply_readmission(owner_vk, submission)
        } else {
            insert_member_room(owner_vk, signing_key, submission)
        };
        if let Err(f) = applied {
            // The messages go back to the composer; nothing was posted.
            return fail(generation, f);
        }
        remember_entry(owner_vk, entry);
        step(|m| m.succeed(generation));
        if !readmit {
            crate::public_room::close();
            // The room now holds the key (`self_sk`, which signs when the
            // delegate cannot). Store it through the delegate like an invited
            // member's, once the room exists, so an abandoned join never
            // leaves an authoritative key for a room the user does not hold.
            #[cfg(not(feature = "no-sync"))]
            crate::util::safe_spawn_local(async move {
                let room_key = owner_vk.to_bytes();
                let stored = crate::signing::migrate_signing_key(room_key, &stored_key, true).await;
                if stored == crate::signing::MigrationResult::Failed {
                    dioxus::logger::tracing::warn!(
                        "public join: could not store the member key in the delegate"
                    );
                }
            });
            #[cfg(feature = "no-sync")]
            drop(stored_key);
        }
    });
}

/// `delta` applied to `live`, the state as it is now, and checked: the room
/// must still hold the entry and every message afterwards. A newer joiner, a ban
/// or a policy change can make its own cleanup drop ours, and then the join did
/// not happen. There is no fallback to the state the submission was built from:
/// that would overwrite what arrived meanwhile.
fn apply_to_live(
    live: &ChatRoomStateV1,
    owner_vk: &VerifyingKey,
    submission: &Submission,
) -> Result<ChatRoomStateV1, JoinFailure> {
    let params = ChatRoomParametersV1 { owner: *owner_vk };
    let mut next = live.clone();
    next.apply_delta(live, &params, &Some(submission.delta.clone()))
        .map_err(|e| JoinFailure::Internal(format!("{e:?}")))?;
    let holds_messages = submission
        .message_ids
        .iter()
        .all(|id| next.recent_messages.messages.iter().any(|m| m.id() == *id));
    let holds_entry = next
        .provisional
        .entries
        .iter()
        .any(|e| e.provisional.member_vk == submission.member_vk);
    if !holds_messages || !holds_entry {
        return Err(JoinFailure::RoomFull);
    }
    next.recent_messages.rebuild_actions_state();
    Ok(next)
}

/// The member room for a completed join: the viewed state as it is now with
/// the entry and messages applied, holding the new key.
fn build_room_data(
    owner_vk: VerifyingKey,
    signing_key: SigningKey,
    state: ChatRoomStateV1,
    submission: Submission,
) -> crate::room_data::RoomData {
    use river_core::chat_delegate::RoomKey;
    let _: RoomKey = owner_vk.to_bytes();
    crate::room_data::RoomData {
        owner_vk,
        room_state: state,
        self_vk: Some(signing_key.verifying_key()),
        self_sk: Some(signing_key),
        contract_key: crate::util::owner_vk_to_contract_key(&owner_vk),
        last_read_message_id: None,
        secrets: Default::default(),
        current_secret_version: None,
        last_secret_rotation: None,
        key_migrated_to_delegate: false,
        self_authorized_member: None,
        invite_chain: vec![],
        self_member_info: Some(submission.info),
        self_nickname: Some(submission.nickname),
        previous_contract_key: None,
        rekey_merge: Default::default(),
        own_purges: Default::default(),
        invitation_secrets: Default::default(),
    }
}

/// The pruned member's own room takes the admission: the entry, the
/// `member_info` and the messages, applied once to the room as it is now, so
/// updates that arrived while the proof was solved are kept. An error leaves
/// the room untouched.
fn apply_readmission(owner_vk: VerifyingKey, submission: Submission) -> Result<(), JoinFailure> {
    use crate::components::app::{mark_needs_sync, ROOMS};
    let result = ROOMS.with_mut(|rooms| {
        let Some(room) = rooms.map.get_mut(&owner_vk) else {
            // The user left the room while the proof was solved.
            return Err(JoinFailure::Internal(
                "this room is no longer in your list".to_string(),
            ));
        };
        let next = apply_to_live(&room.room_state, &owner_vk, &submission)?;
        room.room_state = next;
        room.self_member_info = Some(submission.info);
        room.self_nickname = Some(submission.nickname);
        Ok(())
    });
    if result.is_ok() {
        mark_needs_sync(owner_vk);
    }
    result
}

/// Insert the joined room. Never replaces a room already in `ROOMS`, and is
/// refused, leaving `ROOMS` untouched, when the viewed room is gone or its
/// current state does not keep the entry and messages.
fn insert_member_room(
    owner_vk: VerifyingKey,
    signing_key: SigningKey,
    submission: Submission,
) -> Result<(), JoinFailure> {
    use crate::components::app::{mark_needs_sync, CURRENT_ROOM, ROOMS};
    let live = viewed_state()
        .and_then(|(o, s)| (o == owner_vk).then_some(s))
        .ok_or(JoinFailure::NotOpen)?;
    let state = apply_to_live(&live, &owner_vk, &submission)?;
    let data = build_room_data(owner_vk, signing_key, state, submission);
    let inserted = ROOMS.with_mut(|rooms| match rooms.map.entry(owner_vk) {
        std::collections::hash_map::Entry::Occupied(_) => false,
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(data);
            true
        }
    });
    if !inserted {
        return Err(JoinFailure::AlreadyMember);
    }
    CURRENT_ROOM.with_mut(|current| current.owner_key = Some(owner_vk));
    mark_needs_sync(owner_vk);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::provisional::{
        AuthorizedProvisionalBan, PowPolicy, ProvisionalBan, PublicRoomPolicy,
    };
    use std::time::Duration;

    fn owner() -> SigningKey {
        SigningKey::from_bytes(&[3u8; 32])
    }

    fn policy(bits: u8, max_provisional: u32) -> PublicRoomPolicy {
        PublicRoomPolicy {
            listed: false,
            max_provisional,
            max_provisional_messages: 4,
            max_provisional_bans: 4,
            pow: Some(PowPolicy {
                difficulty_bits: bits,
                epoch: 9,
            }),
        }
    }

    fn room(privacy: PrivacyMode, policy: Option<PublicRoomPolicy>) -> ChatRoomStateV1 {
        let owner = owner();
        let config = Configuration {
            owner_member_id: MemberId::from(&owner.verifying_key()),
            privacy_mode: privacy,
            max_recent_messages: 10,
            public_admission: policy,
            ..Default::default()
        };
        ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(config, &owner),
            ..Default::default()
        }
    }

    fn solved(
        state: &ChatRoomStateV1,
        sk: &SigningKey,
    ) -> (VerifyingKey, AuthorizedProvisionalMember) {
        let owner_vk = owner().verifying_key();
        let req = check_admission_status(
            state,
            &owner_vk,
            &sk.verifying_key(),
            None,
            SystemTime::now(),
        )
        .expect("the room admits");
        let mut solver = PowSolver::new(&owner_vk, &sk.verifying_key(), &req, req.difficulty_bits);
        loop {
            if let PowStep::Found { nonce, .. } = solver.step(5_000) {
                return (owner_vk, entry_for_nonce(&owner_vk, sk, nonce));
            }
        }
    }

    #[test]
    fn join_is_offered_only_by_a_public_room_that_admits_by_pow() {
        assert!(offers_join(&room(PrivacyMode::Public, Some(policy(4, 2)))));
        assert!(!offers_join(&room(PrivacyMode::Public, None)));
        assert!(!offers_join(&room(
            PrivacyMode::Private,
            Some(policy(4, 2))
        )));
        let mut nobody = policy(4, 2);
        nobody.pow = None;
        assert!(!offers_join(&room(PrivacyMode::Public, Some(nobody))));
    }

    #[test]
    fn a_submission_is_a_delta_the_room_keeps_with_the_message_once() {
        let state = room(PrivacyMode::Public, Some(policy(6, 2)));
        let sk = SigningKey::from_bytes(&[11u8; 32]);
        let (owner_vk, entry) = solved(&state, &sk);
        let now = SystemTime::now();
        let sub = build_submission(&state, &owner_vk, &sk, &entry, "hello", "Visitor", now)
            .expect("admits");
        let after = &sub.state_after;
        assert_eq!(after.provisional.entries.len(), 1);
        assert_eq!(after.recent_messages.messages.len(), 1);
        // The delta carries the entry, the member_info and the one message.
        assert_eq!(sub.delta.recent_messages.as_ref().map(|m| m.len()), Some(1));
        assert!(sub.delta.member_info.is_some());
        // Applying it to the room as it was gives the same room, with the
        // message held once.
        let mut again = state.clone();
        again
            .apply_delta(
                &state,
                &ChatRoomParametersV1 { owner: owner_vk },
                &Some(sub.delta.clone()),
            )
            .unwrap();
        assert_eq!(again.recent_messages.messages.len(), 1);
        // And once more is a no-op: exactly once.
        let before = again.recent_messages.messages.len();
        let snapshot = again.clone();
        again
            .apply_delta(
                &snapshot,
                &ChatRoomParametersV1 { owner: owner_vk },
                &Some(sub.delta),
            )
            .unwrap();
        assert_eq!(again.recent_messages.messages.len(), before);
    }

    #[test]
    fn a_banned_key_is_refused_with_its_own_reason() {
        let mut state = room(PrivacyMode::Public, Some(policy(2, 2)));
        let sk = SigningKey::from_bytes(&[12u8; 32]);
        let owner_vk = owner().verifying_key();
        state.provisional.bans.push(AuthorizedProvisionalBan::new(
            &owner_vk,
            ProvisionalBan {
                banned_by: MemberId::from(&owner_vk),
                banned_at: SystemTime::now().into(),
                target: MemberId::from(&sk.verifying_key()),
            },
            &owner(),
        ));
        assert_eq!(
            check_admission_status(
                &state,
                &owner_vk,
                &sk.verifying_key(),
                None,
                SystemTime::now()
            ),
            Err(JoinFailure::Banned)
        );
    }

    #[test]
    fn a_room_that_stops_admitting_is_not_open() {
        let state = room(PrivacyMode::Public, None);
        let sk = SigningKey::from_bytes(&[13u8; 32]);
        assert_eq!(
            check_admission_status(
                &state,
                &owner().verifying_key(),
                &sk.verifying_key(),
                None,
                SystemTime::now()
            ),
            Err(JoinFailure::NotOpen)
        );
    }

    #[test]
    fn a_proof_for_an_old_epoch_is_policy_changed() {
        let state = room(PrivacyMode::Public, Some(policy(6, 2)));
        let sk = SigningKey::from_bytes(&[14u8; 32]);
        let (owner_vk, entry) = solved(&state, &sk);
        // The owner moves to a new epoch: every earlier proof is void.
        let mut moved = policy(6, 2);
        moved.pow = Some(PowPolicy {
            difficulty_bits: 6,
            epoch: 10,
        });
        let new_state = room(PrivacyMode::Public, Some(moved));
        assert_eq!(
            build_submission(
                &new_state,
                &owner_vk,
                &sk,
                &entry,
                "x",
                "n",
                SystemTime::now()
            )
            .unwrap_err(),
            JoinFailure::PolicyChanged
        );
    }

    #[test]
    fn a_room_whose_slots_are_held_by_newer_messages_is_full() {
        // One open-member slot, held by an author whose message is newer than
        // ours.
        let mut state = room(PrivacyMode::Public, Some(policy(2, 1)));
        let other = SigningKey::from_bytes(&[15u8; 32]);
        let (owner_vk, other_entry) = solved(&state, &other);
        let future = SystemTime::now() + Duration::from_secs(3600);
        let held = build_submission(
            &state,
            &owner_vk,
            &other,
            &other_entry,
            "first",
            "Other",
            future,
        )
        .expect("the first author is admitted");
        state = held.state_after;
        let sk = SigningKey::from_bytes(&[16u8; 32]);
        let (_, entry) = solved(&state, &sk);
        let err = build_submission(
            &state,
            &owner_vk,
            &sk,
            &entry,
            "late",
            "Me",
            SystemTime::now(),
        )
        .unwrap_err();
        assert_eq!(err, JoinFailure::RoomFull);
        assert!(!err.retryable() || err.message().contains("later"));
    }

    #[test]
    fn a_message_over_the_room_limit_is_refused_and_not_lost() {
        let state = room(PrivacyMode::Public, Some(policy(2, 2)));
        let sk = SigningKey::from_bytes(&[17u8; 32]);
        let (owner_vk, entry) = solved(&state, &sk);
        let long = "x".repeat(state.configuration.configuration.max_message_size + 1);
        let err = build_submission(
            &state,
            &owner_vk,
            &sk,
            &entry,
            &long,
            "n",
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(matches!(err, JoinFailure::TooLong(_)));
    }

    #[test]
    fn a_queued_message_returns_to_the_draft_in_front_of_new_typing() {
        assert_eq!(restore_draft("", Some("hello")), "hello");
        assert_eq!(restore_draft("   ", Some("hello")), "hello");
        assert_eq!(restore_draft("more", Some("hello")), "hello\nmore");
        assert_eq!(restore_draft("typed", None), "typed");
    }

    #[test]
    fn the_new_room_holds_the_key_and_the_message_and_survives_the_delegate_blob() {
        let state = room(PrivacyMode::Public, Some(policy(2, 2)));
        let sk = SigningKey::from_bytes(&[18u8; 32]);
        let (owner_vk, entry) = solved(&state, &sk);
        let sub = build_submission(
            &state,
            &owner_vk,
            &sk,
            &entry,
            "hi",
            "Me",
            SystemTime::now(),
        )
        .unwrap();
        let data = build_room_data(owner_vk, sk.clone(), sub.state_after.clone(), sub);
        assert_eq!(data.self_verifying_key(), Some(sk.verifying_key()));
        assert!(
            data.can_send_message().is_ok(),
            "a provisional member can post"
        );
        assert!(data.can_participate().is_ok());
        // Persisted like every room: the blob keeps the entry and the message.
        let bytes = crate::util::to_cbor_vec(&data);
        let back: crate::room_data::RoomData =
            crate::util::try_from_cbor_slice(&bytes).expect("decodes");
        assert_eq!(back.room_state.provisional.entries.len(), 1);
        assert_eq!(back.room_state.recent_messages.messages.len(), 1);
        assert_eq!(back.self_verifying_key(), Some(sk.verifying_key()));
    }

    /// A joined member whose entry and messages the room then pruned.
    fn pruned_member() -> (
        crate::room_data::RoomData,
        SigningKey,
        AuthorizedProvisionalMember,
    ) {
        let state = room(PrivacyMode::Public, Some(policy(6, 2)));
        let sk = SigningKey::from_bytes(&[21u8; 32]);
        let (owner_vk, entry) = solved(&state, &sk);
        let sub = build_submission(
            &state,
            &owner_vk,
            &sk,
            &entry,
            "before",
            "Me",
            SystemTime::now(),
        )
        .unwrap();
        let mut data = build_room_data(owner_vk, sk.clone(), sub.state_after.clone(), sub);
        assert!(!data.is_pruned_open_member(), "held, so not pruned");
        data.room_state.provisional.entries.clear();
        data.room_state.recent_messages.messages.clear();
        (data, sk, entry)
    }

    #[test]
    fn a_pruned_open_member_is_told_and_a_held_proof_is_resubmitted_with_the_same_key() {
        let (data, sk, entry) = pruned_member();
        assert!(data.is_pruned_open_member());
        assert!(
            data.can_send_message().is_err(),
            "the normal composer is not offered"
        );
        let owner_vk = data.owner_vk;
        let now = SystemTime::now();
        let note = readmit_note(
            &data.room_state,
            &owner_vk,
            &sk.verifying_key(),
            Some(&entry),
            now,
        );
        assert!(note.contains("Your next message rejoins."), "{note}");
        // The held proof still passes, so it goes out as it is.
        let sub = build_submission(&data.room_state, &owner_vk, &sk, &entry, "again", "Me", now)
            .expect("re-admitted");
        let keys: Vec<_> = sub
            .state_after
            .provisional
            .entries
            .iter()
            .map(|e| e.provisional.member_vk)
            .collect();
        assert_eq!(keys, vec![sk.verifying_key()], "the same identity");
        assert_eq!(sub.state_after.recent_messages.messages.len(), 1);
    }

    #[test]
    fn a_proof_the_policy_no_longer_accepts_is_solved_again_with_the_same_key() {
        let (mut data, sk, old_entry) = pruned_member();
        let owner_vk = data.owner_vk;
        // The owner moves to a new epoch while the member was away.
        let mut moved = policy(6, 2);
        moved.pow = Some(PowPolicy {
            difficulty_bits: 6,
            epoch: 77,
        });
        let newer = room(PrivacyMode::Public, Some(moved));
        data.room_state.configuration = newer.configuration;
        let now = SystemTime::now();
        let note = readmit_note(
            &data.room_state,
            &owner_vk,
            &sk.verifying_key(),
            Some(&old_entry),
            now,
        );
        assert!(note.contains("proof of work"), "{note}");
        assert_eq!(
            build_submission(&data.room_state, &owner_vk, &sk, &old_entry, "x", "Me", now)
                .unwrap_err(),
            JoinFailure::PolicyChanged
        );
        // Solved again for the new epoch, by the same key.
        let (_, fresh) = solved(&data.room_state, &sk);
        assert_eq!(fresh.provisional.member_vk, sk.verifying_key());
        let sub = build_submission(&data.room_state, &owner_vk, &sk, &fresh, "back", "Me", now)
            .expect("re-admitted");
        assert_eq!(sub.state_after.provisional.entries.len(), 1);
        assert_eq!(sub.state_after.recent_messages.messages.len(), 1);
    }

    #[test]
    fn re_admission_gives_the_rooms_own_reason_when_it_cannot() {
        let (mut data, sk, entry) = pruned_member();
        let owner_vk = data.owner_vk;
        let now = SystemTime::now();
        // Banned by the owner while away.
        data.room_state
            .provisional
            .bans
            .push(AuthorizedProvisionalBan::new(
                &owner_vk,
                ProvisionalBan {
                    banned_by: MemberId::from(&owner_vk),
                    banned_at: now.into(),
                    target: MemberId::from(&sk.verifying_key()),
                },
                &owner(),
            ));
        let note = readmit_note(
            &data.room_state,
            &owner_vk,
            &sk.verifying_key(),
            Some(&entry),
            now,
        );
        assert_eq!(note, JoinFailure::Banned.message());
    }

    #[test]
    fn a_member_who_joined_by_invitation_is_not_a_pruned_open_member() {
        let (mut data, _, _) = pruned_member();
        data.self_authorized_member = Some(river_core::room_state::member::AuthorizedMember::new(
            river_core::room_state::member::Member {
                owner_member_id: MemberId::from(&data.owner_vk),
                invited_by: MemberId::from(&data.owner_vk),
                member_vk: data.self_verifying_key().unwrap(),
            },
            &owner(),
        ));
        assert!(!data.is_pruned_open_member());
    }

    #[test]
    fn the_member_key_is_never_written_to_browser_storage() {
        let src = include_str!("public_join.rs");
        let production = src.split("#[cfg(test)]").next().unwrap();
        for needle in ["local_storage", "session_storage", "set_item"] {
            assert!(!production.contains(needle), "{needle} in production code");
        }
    }

    // ---- the state machine: exactly-once, queue, scoping, no lost text ----

    fn vk(seed: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    fn limits() -> Limits {
        Limits {
            max_message_size: 100,
            max_queued: 3,
        }
    }

    fn fresh() -> StartPlan {
        StartPlan {
            readmit: false,
            key: Some(SigningKey::generate(&mut rand::thread_rng())),
            entry: None,
            blocker: None,
        }
    }

    fn some_entry() -> AuthorizedProvisionalMember {
        let state = room(PrivacyMode::Public, Some(policy(2, 2)));
        let sk = SigningKey::from_bytes(&[40u8; 32]);
        solved(&state, &sk).1
    }

    fn returned(m: &mut Machine, owner: &VerifyingKey) -> String {
        m.take_returned(owner).unwrap_or_default()
    }

    #[test]
    fn two_sends_while_solving_are_both_queued_and_post_once_in_order() {
        let (a, mut m) = (vk(1), Machine::new());
        let Action::Solve(g) = m.send(a, "one", limits(), fresh()) else {
            panic!("the first send starts the solve");
        };
        assert_eq!(m.send(a, "two", limits(), fresh()), Action::Nothing);
        assert_eq!(m.ui().queued, vec!["one", "two"]);
        // Solved: one Submit for the whole queue.
        assert_eq!(m.solved(g, some_entry()), Action::Submit(g));
        assert!(matches!(m.ui().phase, JoinPhase::Submitting));
        assert_eq!(m.ui().queued, vec!["one", "two"], "pending until posted");
        // The batch can be submitted from `submit_inputs` once; a second
        // `solved` or `send` cannot start another submit.
        assert_eq!(m.solved(g, some_entry()), Action::Nothing);
        assert_eq!(m.send(a, "three", limits(), fresh()), Action::Nothing);
        assert_eq!(
            m.submit_inputs(g).map(|(_, _, t, _)| t),
            Some(vec!["one".to_string(), "two".to_string()])
        );
        // Posted once: succeed returns the batch and a second succeed nothing.
        assert_eq!(m.succeed(g), Some(vec!["one".into(), "two".into()]));
        assert_eq!(m.succeed(g), None);
        // "three" came back to the box it was typed in, it was not posted.
        assert_eq!(returned(&mut m, &a), "three");
    }

    #[test]
    fn two_quick_sends_when_ready_submit_once() {
        let (a, mut m) = (vk(1), Machine::new());
        let Action::Solve(g) = m.start(a, fresh()) else {
            panic!("start solves");
        };
        assert_eq!(m.solved(g, some_entry()), Action::Nothing);
        assert!(matches!(m.ui().phase, JoinPhase::Ready));
        assert_eq!(m.send(a, "x", limits(), fresh()), Action::Submit(g));
        // The phase flipped in the same step: the second send cannot submit.
        assert_eq!(m.send(a, "y", limits(), fresh()), Action::Nothing);
        assert_eq!(m.succeed(g), Some(vec!["x".to_string()]));
        assert_eq!(returned(&mut m, &a), "y");
    }

    #[test]
    fn enter_while_submitting_hands_the_text_back_and_says_why() {
        let (a, mut m) = (vk(1), Machine::new());
        let Action::Solve(g) = m.send(a, "first", limits(), fresh()) else {
            panic!()
        };
        m.solved(g, some_entry());
        m.send(a, "again", limits(), fresh());
        assert_eq!(returned(&mut m, &a), "again");
        assert!(m.ui().notice.is_some());
        assert_eq!(m.ui().queued, vec!["first"], "the batch is untouched");
    }

    #[test]
    fn cancel_hands_every_message_back_and_forgets_the_key() {
        let (a, mut m) = (vk(1), Machine::new());
        let Action::Solve(g) = m.send(a, "one", limits(), fresh()) else {
            panic!()
        };
        m.send(a, "two", limits(), fresh());
        m.cancel();
        assert!(m.signing_key.is_none(), "no key outlives a cancel");
        assert!(matches!(m.ui().phase, JoinPhase::Idle));
        assert_eq!(returned(&mut m, &a), "one\ntwo");
        // The cancelled task finds itself stale and cannot post.
        assert!(!m.is_current(g));
        assert_eq!(m.solved(g, some_entry()), Action::Nothing);
        assert!(m.submit_inputs(g).is_none());
        assert_eq!(m.succeed(g), None);
    }

    #[test]
    fn failure_hands_messages_back_and_forgets_the_key() {
        let (a, mut m) = (vk(1), Machine::new());
        let Action::Solve(g) = m.send(a, "hi", limits(), fresh()) else {
            panic!()
        };
        m.fail(g, JoinFailure::Banned);
        assert!(m.signing_key.is_none());
        assert_eq!(returned(&mut m, &a), "hi");
        assert!(matches!(
            m.ui().phase,
            JoinPhase::Failed(JoinFailure::Banned)
        ));
    }

    #[test]
    fn a_message_that_is_too_long_is_refused_before_any_solving() {
        let (a, mut m) = (vk(1), Machine::new());
        let long = "x".repeat(101);
        assert_eq!(m.send(a, &long, limits(), fresh()), Action::Nothing);
        assert!(matches!(m.ui().phase, JoinPhase::Idle), "no solve started");
        assert!(m.signing_key.is_none(), "no key made");
        assert_eq!(returned(&mut m, &a), long);
        assert!(m.ui().notice.is_some());
    }

    #[test]
    fn the_queue_is_capped_at_what_the_entry_can_carry() {
        let (a, mut m) = (vk(1), Machine::new());
        m.send(a, "1", limits(), fresh());
        m.send(a, "2", limits(), fresh());
        m.send(a, "3", limits(), fresh());
        m.send(a, "4", limits(), fresh());
        assert_eq!(m.ui().queued, vec!["1", "2", "3"]);
        assert_eq!(returned(&mut m, &a), "4");
    }

    #[test]
    fn a_join_for_another_room_never_receives_this_rooms_messages() {
        let (a, b, mut m) = (vk(1), vk(2), Machine::new());
        let Action::Solve(ga) = m.send(a, "for A", limits(), fresh()) else {
            panic!()
        };
        // The user switches to room B and sends there.
        let Action::Solve(gb) = m.send(b, "for B", limits(), fresh()) else {
            panic!("B starts its own join");
        };
        assert_ne!(ga, gb);
        assert_eq!(m.ui().owner, Some(b));
        assert_eq!(m.ui().queued, vec!["for B"]);
        // A's task is stale and A's text went back to A's composer.
        assert_eq!(m.solved(ga, some_entry()), Action::Nothing);
        assert_eq!(returned(&mut m, &a), "for A");
        assert_eq!(m.solved(gb, some_entry()), Action::Submit(gb));
        assert_eq!(m.succeed(gb), Some(vec!["for B".to_string()]));
        // A's composer sees an idle join, B's state is not A's.
        assert_eq!(m.ui().phase_for(&a), JoinPhase::Idle);
    }

    #[test]
    fn nothing_is_posted_after_the_viewer_closes() {
        let (a, mut m) = (vk(1), Machine::new());
        let Action::Solve(g) = m.send(a, "hi", limits(), fresh()) else {
            panic!()
        };
        m.reset();
        assert_eq!(m.solved(g, some_entry()), Action::Nothing);
        assert!(m.submit_inputs(g).is_none());
        assert_eq!(returned(&mut m, &a), "hi");
        assert_eq!(m.ui().owner, None);
    }

    #[test]
    fn a_refused_start_hands_the_text_back_with_the_reason() {
        let (a, mut m) = (vk(1), Machine::new());
        let blocked = StartPlan {
            blocker: Some(JoinFailure::AlreadyMember),
            ..fresh()
        };
        assert_eq!(m.send(a, "hi", limits(), blocked), Action::Nothing);
        assert!(matches!(
            m.ui().phase,
            JoinPhase::Failed(JoinFailure::AlreadyMember)
        ));
        assert_eq!(returned(&mut m, &a), "hi");
        // No key is made for a room that is already held.
        assert!(m.signing_key.is_none());
    }

    #[test]
    fn a_readmission_without_the_members_key_fails_clearly_not_pending() {
        let (a, mut m) = (vk(1), Machine::new());
        let no_key = StartPlan {
            readmit: true,
            key: None,
            entry: None,
            blocker: None,
        };
        assert_eq!(m.send(a, "hi", limits(), no_key), Action::Nothing);
        let JoinPhase::Failed(f) = m.ui().phase else {
            panic!("must not stay pending");
        };
        assert_eq!(f, JoinFailure::NoKey);
        assert!(!f.retryable());
        assert_eq!(returned(&mut m, &a), "hi");
    }

    #[test]
    fn a_proof_that_stops_passing_is_solved_again_keeping_every_message() {
        let (a, mut m) = (vk(1), Machine::new());
        let Action::Solve(g) = m.send(a, "one", limits(), fresh()) else {
            panic!()
        };
        m.solved(g, some_entry());
        let Action::Solve(g2) = m.restart_solving(g) else {
            panic!()
        };
        assert_ne!(g, g2);
        assert_eq!(m.ui().queued, vec!["one"]);
        assert!(matches!(m.ui().phase, JoinPhase::Solving { .. }));
        assert_eq!(m.solved(g2, some_entry()), Action::Submit(g2));
        assert_eq!(m.succeed(g2), Some(vec!["one".to_string()]));
    }

    #[test]
    fn a_draft_in_the_box_follows_a_completed_join_only() {
        let (a, mut m) = (vk(1), Machine::new());
        m.note_draft(a, "left");
        // No join completed: nothing is carried.
        assert_eq!(m.take_carried(&a), None);
        let Action::Solve(g) = m.send(a, "sent", limits(), fresh()) else {
            panic!()
        };
        m.note_draft(a, "left over");
        m.solved(g, some_entry());
        m.succeed(g);
        assert_eq!(m.take_carried(&vk(2)), None);
        assert_eq!(m.take_carried(&a), Some("left over".to_string()));
        assert_eq!(m.take_carried(&a), None);
    }

    #[test]
    fn a_cleared_draft_is_not_carried() {
        let (a, mut m) = (vk(1), Machine::new());
        let Action::Solve(g) = m.send(a, "sent", limits(), fresh()) else {
            panic!()
        };
        m.note_draft(a, "typed");
        m.note_draft(a, "");
        m.solved(g, some_entry());
        m.succeed(g);
        assert_eq!(m.take_carried(&a), None);
    }

    #[test]
    fn several_messages_are_admitted_in_one_delta_and_kept() {
        use river_core::room_state::provisional::PROVISIONAL_MESSAGES_PER_AUTHOR;
        let state = room(PrivacyMode::Public, Some(policy(2, 2)));
        let sk = SigningKey::from_bytes(&[41u8; 32]);
        let (owner_vk, entry) = solved(&state, &sk);
        let texts: Vec<String> = ["a", "b", "c"]
            .iter()
            .take(PROVISIONAL_MESSAGES_PER_AUTHOR)
            .map(|t| t.to_string())
            .collect();
        let sub = build_submission_many(
            &state,
            &owner_vk,
            &sk,
            &entry,
            &texts,
            "Me",
            SystemTime::now(),
        )
        .expect("admits");
        let kept: Vec<String> = sub
            .state_after
            .recent_messages
            .messages
            .iter()
            .map(|m| format!("{:?}", m.message.content))
            .collect();
        assert_eq!(
            kept.len(),
            PROVISIONAL_MESSAGES_PER_AUTHOR,
            "every message once: {kept:?}"
        );
        // One more than the per-author cap: the room keeps only the newest
        // PROVISIONAL_MESSAGES_PER_AUTHOR, so the submission is refused.
        let three: Vec<String> = ["a", "b", "c"].iter().map(|t| t.to_string()).collect();
        let over = build_submission_many(
            &state,
            &owner_vk,
            &sk,
            &entry,
            &three,
            "Me",
            SystemTime::now(),
        );
        assert!(over.is_err(), "a third message does not all stay");
    }

    #[test]
    fn the_live_apply_keeps_what_arrived_and_refuses_when_the_room_drops_ours() {
        let state = room(PrivacyMode::Public, Some(policy(2, 1)));
        let sk = SigningKey::from_bytes(&[50u8; 32]);
        let (owner_vk, entry) = solved(&state, &sk);
        let sub = build_submission(
            &state,
            &owner_vk,
            &sk,
            &entry,
            "mine",
            "Me",
            SystemTime::now(),
        )
        .expect("admits against the snapshot");
        // The room is unchanged: the live apply holds the entry and message.
        let ok = apply_to_live(&state, &owner_vk, &sub).expect("kept");
        assert!(ok
            .recent_messages
            .messages
            .iter()
            .any(|m| m.id() == sub.message_ids[0]));

        // Meanwhile another joiner took the only slot with a NEWER message:
        // the room's cleanup drops our entry. That is a refusal, not a post,
        // and there is no fallback to the stale snapshot.
        let other = SigningKey::from_bytes(&[51u8; 32]);
        let (_, other_entry) = solved(&state, &other);
        let later = SystemTime::now() + Duration::from_secs(3600);
        let live = build_submission(
            &state,
            &owner_vk,
            &other,
            &other_entry,
            "theirs",
            "Other",
            later,
        )
        .expect("the other joiner is admitted")
        .state_after;
        assert_eq!(
            apply_to_live(&live, &owner_vk, &sub).unwrap_err(),
            JoinFailure::RoomFull
        );
    }

    #[test]
    fn a_draft_follows_a_readmission_too() {
        let (a, mut m) = (vk(1), Machine::new());
        let plan = StartPlan {
            readmit: true,
            ..fresh()
        };
        let Action::Solve(g) = m.send(a, "back", limits(), plan) else {
            panic!()
        };
        m.solved(g, some_entry());
        assert_eq!(m.succeed(g), Some(vec!["back".to_string()]));
        m.note_draft(a, "typed while solving");
        assert_eq!(m.take_carried(&a), Some("typed while solving".to_string()));
    }

    #[test]
    fn a_too_long_message_in_a_new_room_does_not_show_the_old_rooms_failure() {
        let (a, b, mut m) = (vk(1), vk(2), Machine::new());
        let Action::Solve(g) = m.send(a, "x", limits(), fresh()) else {
            panic!()
        };
        m.fail(g, JoinFailure::Banned);
        assert!(matches!(m.ui().phase_for(&a), JoinPhase::Failed(_)));
        let long = "y".repeat(101);
        m.send(b, &long, limits(), fresh());
        assert_eq!(m.ui().phase_for(&b), JoinPhase::Idle, "B is not banned");
        assert!(m.ui().notice_for(&b).is_some());
    }
}
