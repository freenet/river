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
            Self::Shadowed => "Another key already holds this identity in the room. \
                Try again to join with a new one."
                .to_string(),
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
            Self::RoomFull | Self::Shadowed | Self::PolicyChanged | Self::Internal(_)
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
}

/// Build the one delta that admits `signing_key` with `text` as its first
/// message, and apply it to a copy of `state`.
///
/// Refuses, with the reason, when the room would not keep the entry or the
/// message: the status is asked of the room's own cleanup, and the result is
/// checked to hold both.
pub fn build_submission(
    state: &ChatRoomStateV1,
    owner_vk: &VerifyingKey,
    signing_key: &SigningKey,
    entry: &AuthorizedProvisionalMember,
    text: &str,
    nickname: &str,
    now: SystemTime,
) -> Result<Submission, JoinFailure> {
    let config = &state.configuration.configuration;
    if text.len() > config.max_message_size {
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
    let message = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: MemberId::from(owner_vk),
            author: id,
            content: RoomMessageBody::public(text.to_string()),
            time: now,
        },
        signing_key,
    );
    let message_id = message.id();
    let delta = admission_delta(state, &params, entry, info.clone(), message);
    let mut state_after = state.clone();
    state_after
        .apply_delta(state, &params, &Some(delta.clone()))
        .map_err(|e| JoinFailure::Internal(format!("{e:?}")))?;
    let kept = state_after
        .recent_messages
        .messages
        .iter()
        .any(|m| m.id() == message_id)
        && state_after
            .provisional
            .entries
            .iter()
            .any(|e| e.provisional.member_vk == member_vk);
    if !kept {
        // The room's cleanup dropped the entry or the message: the pool or the
        // quota is held by newer messages.
        return Err(JoinFailure::RoomFull);
    }
    state_after.recent_messages.rebuild_actions_state();
    Ok(Submission {
        delta,
        state_after,
        info,
        nickname: nickname.to_string(),
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

/// What the view shows. Nothing secret.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinUi {
    pub phase: JoinPhase,
    /// A message sent before admission: shown as pending, posted once.
    pub queued: Option<String>,
    /// A queued message handed back after a failure or cancel. The composer
    /// puts it back in its draft box (in front of anything typed since) and
    /// clears this.
    pub returned: Option<String>,
}

impl Default for JoinUi {
    fn default() -> Self {
        Self {
            phase: JoinPhase::Idle,
            queued: None,
            returned: None,
        }
    }
}

pub static JOIN: GlobalSignal<JoinUi> = Global::new(JoinUi::default);

/// The member key and proof of the join in progress. Process memory only.
struct Session {
    /// Increases on every start and cancel; a task that finds another value
    /// stops.
    generation: u64,
    owner_vk: Option<VerifyingKey>,
    signing_key: Option<SigningKey>,
    entry: Option<AuthorizedProvisionalMember>,
    /// Re-admitting a member already in `ROOMS` (pruned), not joining from the
    /// viewer: the key is the room's own and the state is the room's.
    readmit: bool,
}

static SESSION: Mutex<Session> = Mutex::new(Session {
    generation: 0,
    owner_vk: None,
    signing_key: None,
    entry: None,
    readmit: false,
});

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
fn source_state(owner_vk: &VerifyingKey) -> Option<ChatRoomStateV1> {
    if session().readmit {
        let rooms = crate::components::app::ROOMS.try_peek().ok()?;
        return rooms.map.get(owner_vk).map(|r| r.room_state.clone());
    }
    viewed_state().map(|(_, s)| s)
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

fn session() -> std::sync::MutexGuard<'static, Session> {
    SESSION.lock().unwrap_or_else(|e| e.into_inner())
}

fn is_current(generation: u64) -> bool {
    session().generation == generation
}

/// Write the UI state from a clean execution context.
fn update_ui(f: impl FnOnce(&mut JoinUi) + 'static) {
    crate::util::defer(move || {
        JOIN.with_mut(f);
    });
}

/// A queued message goes back to the composer: it is never lost.
fn hand_back(ui: &mut JoinUi) {
    if let Some(q) = ui.queued.take() {
        ui.returned = Some(restore_draft(
            ui.returned.as_deref().unwrap_or(""),
            Some(&q),
        ));
    }
}

fn fail(generation: u64, failure: JoinFailure) {
    if !is_current(generation) {
        return;
    }
    update_ui(move |ui| {
        hand_back(ui);
        ui.phase = JoinPhase::Failed(failure);
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
    session().signing_key.as_ref().map(|k| k.verifying_key())
}

/// Forget the join: it is over, or the viewer closed.
pub fn reset() {
    {
        let mut s = session();
        s.generation += 1;
        s.owner_vk = None;
        s.signing_key = None;
        s.entry = None;
        s.readmit = false;
    }
    update_ui(|ui| *ui = JoinUi::default());
}

/// Cancel solving. The draft, and any queued message, return to the draft box.
pub fn cancel() {
    {
        let mut s = session();
        s.generation += 1;
        s.signing_key = None;
        s.entry = None;
    }
    update_ui(|ui| {
        hand_back(ui);
        ui.phase = JoinPhase::Idle;
    });
}

/// Start admission (Join pressed, or Send pressed before admission). A
/// no-op while one is under way.
pub fn start(owner_vk: VerifyingKey) {
    let generation = {
        let mut s = session();
        if matches!(
            JOIN.try_peek().map(|u| u.phase.clone()),
            Ok(JoinPhase::Solving { .. })
        ) {
            return;
        }
        s.generation += 1;
        s.owner_vk = Some(owner_vk);
        // A fresh member key for every attempt. The old one, if any, is
        // forgotten: a failed or cancelled attempt posted nothing under it.
        s.signing_key = Some(SigningKey::generate(&mut rand::thread_rng()));
        s.entry = None;
        s.readmit = false;
        s.generation
    };
    update_ui(|ui| {
        ui.phase = JoinPhase::Solving {
            tried: 0,
            expected: 0.0,
        };
    });
    crate::util::safe_spawn_local(async move {
        run(generation, owner_vk).await;
    });
}

/// Re-admit the member of a room the room pruned, with the SAME key: the held
/// proof is re-submitted when the policy still accepts it, otherwise the proof
/// is solved again. A no-op while one is under way.
pub fn start_readmit(owner_vk: VerifyingKey) {
    let Some(signing_key) = crate::components::app::ROOMS
        .try_peek()
        .ok()
        .and_then(|r| r.map.get(&owner_vk).and_then(|d| d.signing_key().cloned()))
    else {
        return;
    };
    let generation = {
        let mut s = session();
        if matches!(
            JOIN.try_peek().map(|u| u.phase.clone()),
            Ok(JoinPhase::Solving { .. })
        ) {
            return;
        }
        s.generation += 1;
        s.owner_vk = Some(owner_vk);
        s.entry = held_entry(&owner_vk);
        s.signing_key = Some(signing_key);
        s.readmit = true;
        s.generation
    };
    update_ui(|ui| {
        ui.phase = JoinPhase::Solving {
            tried: 0,
            expected: 0.0,
        };
    });
    crate::util::safe_spawn_local(async move {
        run(generation, owner_vk).await;
    });
}

/// Send pressed with `text`: queue it, and start admission or post it.
pub fn send(owner_vk: VerifyingKey, text: String) {
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let phase = JOIN.try_peek().map(|u| u.phase.clone()).ok();
    let ready = matches!(phase, Some(JoinPhase::Ready));
    let solving = matches!(phase, Some(JoinPhase::Solving { .. }));
    let submitting = matches!(phase, Some(JoinPhase::Submitting));
    if submitting {
        return;
    }
    let queued = text.clone();
    update_ui(move |ui| ui.queued = Some(queued));
    if ready {
        let generation = session().generation;
        crate::util::safe_spawn_local(async move {
            submit(generation, owner_vk, text).await;
        });
    } else if !solving {
        // Not viewing the room: the sender is a member the room pruned.
        if viewed_state().is_some() {
            start(owner_vk);
        } else {
            start_readmit(owner_vk);
        }
    }
}

async fn run(generation: u64, owner_vk: VerifyingKey) {
    let Some(signing_key) = session().signing_key.clone() else {
        return;
    };
    let member_vk = signing_key.verifying_key();

    // The key goes through the delegate like an invited member's, so the hosted
    // delegate's quota applies and the key is the one used afterwards. The
    // example build has no delegate.
    #[cfg(not(feature = "no-sync"))]
    if !session().readmit {
        let room_key = owner_vk.to_bytes();
        let stored = crate::signing::migrate_signing_key(room_key, &signing_key, true).await;
        if stored == crate::signing::MigrationResult::Failed {
            // The same fallback as an invitation: the key is also kept in the
            // room's own record, which signs when the delegate cannot.
            dioxus::logger::tracing::warn!(
                "public join: could not store the member key in the delegate"
            );
        }
        if !is_current(generation) {
            return;
        }
    }

    let Some(state) = source_state(&owner_vk) else {
        fail(generation, JoinFailure::NotOpen);
        return;
    };
    // A proof held from earlier is re-submitted when the room's current policy
    // still accepts it; otherwise it is solved again with the same key.
    let held = session().entry.clone();
    if let Some(entry) = held {
        let now = crate::util::get_current_system_time();
        match check_admission_status(&state, &owner_vk, &member_vk, Some(&entry), now) {
            Ok(_) => return finish(generation, owner_vk).await,
            Err(JoinFailure::PolicyChanged) => session().entry = None,
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
        match solver.step(SLICE) {
            PowStep::Found { nonce, .. } => break nonce,
            PowStep::Progress { tried, expected } => {
                slices += 1;
                if slices % PROGRESS_EVERY == 1 {
                    update_ui(move |ui| {
                        if matches!(ui.phase, JoinPhase::Solving { .. }) {
                            ui.phase = JoinPhase::Solving { tried, expected };
                        }
                    });
                }
            }
        }
        // The room may change what it asks of newcomers while this runs
        // (a new epoch restarts the search, more bits raise the target); one
        // that no longer admits stops it.
        if let Some(now_state) = source_state(&owner_vk) {
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
    session().entry = Some(entry);
    finish(generation, owner_vk).await
}

/// The proof is held: post the queued message, or wait for the first one.
async fn finish(generation: u64, owner_vk: VerifyingKey) {
    let queued = JOIN.try_peek().ok().and_then(|u| u.queued.clone());
    match queued {
        Some(text) => submit(generation, owner_vk, text).await,
        None => update_ui(|ui| {
            if matches!(ui.phase, JoinPhase::Solving { .. }) {
                ui.phase = JoinPhase::Ready;
            }
        }),
    }
}

/// Post the entry with `text`, and make the room the visitor's own.
async fn submit(generation: u64, owner_vk: VerifyingKey, text: String) {
    let (signing_key, entry) = {
        let s = session();
        match (s.signing_key.clone(), s.entry.clone()) {
            (Some(k), Some(e)) if s.generation == generation => (k, e),
            _ => return,
        }
    };
    update_ui(|ui| ui.phase = JoinPhase::Submitting);
    let Some(state) = source_state(&owner_vk) else {
        return fail(generation, JoinFailure::NotOpen);
    };
    let nickname = crate::nickname::generate_default_nickname(&signing_key.verifying_key());
    let submission = match build_submission(
        &state,
        &owner_vk,
        &signing_key,
        &entry,
        &text,
        &nickname,
        crate::util::get_current_system_time(),
    ) {
        Ok(s) => s,
        Err(JoinFailure::PolicyChanged) => {
            // The proof no longer passes: solve again, keeping the queued
            // message, instead of dropping the visitor into an error.
            session().entry = None;
            session().generation += 1;
            let generation = session().generation;
            update_ui(|ui| {
                ui.phase = JoinPhase::Solving {
                    tried: 0,
                    expected: 0.0,
                }
            });
            crate::util::safe_spawn_local(async move { run(generation, owner_vk).await });
            return;
        }
        Err(f) => return fail(generation, f),
    };
    if !is_current(generation) {
        return;
    }
    let readmit = session().readmit;
    remember_entry(owner_vk, entry);
    crate::util::defer(move || {
        if readmit {
            apply_readmission(owner_vk, submission);
        } else {
            insert_member_room(owner_vk, signing_key, submission);
            crate::public_room::close();
        }
        reset();
    });
}

/// The member room for a completed join: the viewed state with the entry and
/// first message applied, holding the new key.
fn build_room_data(
    owner_vk: VerifyingKey,
    signing_key: SigningKey,
    submission: Submission,
) -> crate::room_data::RoomData {
    use river_core::chat_delegate::RoomKey;
    let _: RoomKey = owner_vk.to_bytes();
    crate::room_data::RoomData {
        owner_vk,
        room_state: submission.state_after,
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
/// `member_info` and the message, applied once.
fn apply_readmission(owner_vk: VerifyingKey, submission: Submission) {
    use crate::components::app::{mark_needs_sync, ROOMS};
    ROOMS.with_mut(|rooms| {
        if let Some(room) = rooms.map.get_mut(&owner_vk) {
            room.room_state = submission.state_after;
            room.self_member_info = Some(submission.info);
            room.self_nickname = Some(submission.nickname);
        }
    });
    mark_needs_sync(owner_vk);
}

fn insert_member_room(owner_vk: VerifyingKey, signing_key: SigningKey, submission: Submission) {
    use crate::components::app::{mark_needs_sync, CURRENT_ROOM, ROOMS};
    let data = build_room_data(owner_vk, signing_key, submission);
    ROOMS.with_mut(|rooms| {
        rooms.map.insert(owner_vk, data);
    });
    CURRENT_ROOM.with_mut(|current| current.owner_key = Some(owner_vk));
    mark_needs_sync(owner_vk);
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
                banned_at: SystemTime::now(),
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
        let data = build_room_data(owner_vk, sk.clone(), sub);
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
        let mut data = build_room_data(owner_vk, sk.clone(), sub);
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
                    banned_at: now,
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
}
