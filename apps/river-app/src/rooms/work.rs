//! Creating a room and joining one from an invitation: the steps each takes on the node, one
//! request at a time, and the only writes this app makes to the chat delegate's storage.
//!
//! **The steps.** Creating: store the room, publish it (the seed PUT, the one PUT this app
//! makes, of the new room's own contract), then point `rooms_meta` at it. Joining: read the
//! room from the network (a GET), build what joining sends ([`super::create::join`]), store the
//! room, send the join as an UPDATE, then point `rooms_meta` at it. The room is stored before
//! anything reaches the network, so its key is never on the network without being in the
//! store. (The web UI sends first and saves right after, without waiting for an answer.) A
//! room stored whose publish or join then fails is still the user's: the web UI seeds a room
//! the network lacks when it next loads it, and a stored invitee who is not yet a member is
//! re-added when they next send (`build_rejoin_delta`, here and in the web UI).
//!
//! **Writing the store beside the web UI.** Each write is the web UI's own read-merge-write
//! compare-and-swap (`cas_write_key` in `ui/src/components/app/chat_delegate.rs`): read the key
//! with its generation (`GetVersionedRequest`), work out what to store from what is there,
//! store it only if the generation is still the one read (`CasStoreRequest`), and on a
//! conflict read again and work it out again, at most [`CAS_ATTEMPTS`] times, as the web UI's
//! `ROOMS_CAS_MAX_ATTEMPTS`. What to store comes from `river_core::stored_rooms`: a room is
//! only ever added where its key holds nothing (or a tombstone, when joining, as the web UI's
//! explicit rejoin overwrites one), never over a room already stored, and `rooms_meta` has only
//! its open room changed, with every other field kept. A conflict never overwrites: what the
//! other writer stored is read and worked from.
//!
//! **Matching answers.** Delegate answers carry no request id, and neither do contract
//! answers. So one task works at a time, with one request out, and an answer is taken only if
//! it is the kind awaited, for the key awaited: a `GetVersionedResponse` or `CasStoreResponse`
//! for the delegate key, a GET, PUT or UPDATE answer for the room's contract. Nothing else on
//! the connection sends those: the stored rooms are read with `GetRequest`, and the session
//! does not start reading them while a task is working, nor a task while a read is out; while
//! a join's room is shown, nothing else is sent to it. An error that names nothing is taken
//! only when nothing else could have caused it. A request unanswered for its deadline drops
//! the connection, so its answer can never arrive late and be taken for the next one's; every
//! step can be taken again from its start (reading again, sending the same bytes again), so
//! the task goes on on the next connection, up to [`MAX_RECONNECTS`] times.
//!
//! **What the user sees.** A task working, failed (with why, Try again and Dismiss) or done
//! ([`WorkView`]). Nothing fails without saying so.
//!
//! What this leaves to the web UI: storing the room's key in the delegate's `signing_key:`
//! namespace and, for a room the user owns, asking the delegate to follow it
//! (`EnsureRoomSubscription`). The web UI does both for every stored room each time it loads
//! (`hydrate_loaded_rooms_with_authority` in `freenet_api/response_handler.rs`), and the room
//! slot holds everything they are rebuilt from.

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{
    ClientError, ClientRequest, ContractError, ContractRequest, ContractResponse, DelegateError,
    ErrorKind, RequestError,
};
use freenet_stdlib::prelude::{ContractInstanceId, StateDelta, UpdateData};
use river_core::chat_delegate::{CasStoreResult, ChatDelegateResponseMsg};
use river_core::invitation::{Invitation, invitation_text_from_paste};
use river_core::room_state::ChatRoomStateV1;
use river_core::stored_rooms::{
    NewRoom, NewRoomWrite, ROOMS_META_KEY, StoredRoom, StoredRoomSlot, decode_room_slot,
    encode_new_room, meta_with_current_room, reconcile_new_room, room_storage_key,
};

use super::create::{self, JoinPlan, NewRoomPlan};
use super::delegate;

/// How many times a write is worked out again after another writer got in first. The web UI's
/// `ROOMS_CAS_MAX_ATTEMPTS`.
pub const CAS_ATTEMPTS: u32 = 8;

/// How many connections a task may drop because a request went unanswered before it fails.
pub const MAX_RECONNECTS: u32 = 3;

/// The room as the stored-room reader reads it back.
pub fn stored(room: &NewRoom) -> StoredRoom {
    match encode_new_room(room).map(|b| decode_room_slot(&b)) {
        Ok(Ok(StoredRoomSlot::Present(r))) => *r,
        // `NewRoom` encodes every field `StoredRoom` reads, under the same names (pinned by
        // `a_new_room_reads_back_as_itself`); this only keeps the function total.
        _ => StoredRoom {
            owner_vk: room.owner_vk,
            room_state: room.room_state.clone(),
            self_sk: room.self_sk.clone(),
            self_vk: room.self_vk,
            contract_key: room.contract_key,
            last_read_message_id: room.last_read_message_id.clone(),
            self_nickname: room.self_nickname.clone(),
            invitation_secrets: room.invitation_secrets.clone(),
            self_authorized_member: room.self_authorized_member.clone(),
            invite_chain: room.invite_chain.clone(),
            self_member_info: room.self_member_info.clone(),
        },
    }
}

/// A task as the screen shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkView {
    pub id: u64,
    /// What it is, for the screen: "Creating “Book club”", "Joining a room".
    pub what: String,
    pub state: WorkState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkState {
    Working,
    /// Why, and whether Try again can help.
    Failed {
        why: String,
        retry: bool,
    },
    /// Done: the room (by key) is stored and on its way into the list.
    Done {
        room: String,
    },
}

enum Kind {
    Create(Box<NewRoomPlan>),
    Join {
        invitation: Box<Invitation>,
        nickname: String,
        /// Built when the room is read, so its join event carries that time. Kept across
        /// connections, so a resend is the same bytes; built again on Try again, so a join
        /// retried much later is not stamped with a time the room has trimmed.
        plan: Option<Box<JoinPlan>>,
    },
    /// Refused before it started; never works.
    Refused(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Read the room from the network (joining).
    Get,
    /// Add the room to the store.
    Slot,
    /// Publish a new room.
    Put,
    /// Send the join.
    Update,
    /// Point `rooms_meta` at the room.
    Meta,
    Done,
}

/// Where a write (the `Slot` and `Meta` steps) stands.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    /// Read the key and its generation.
    Read,
    /// Store these bytes if the key is still at this generation.
    Store { value: Vec<u8>, generation: u64 },
}

/// What a task has out on the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Out {
    /// A `GetVersionedRequest` for this delegate key.
    Read(Vec<u8>),
    /// A `CasStoreRequest` for this delegate key.
    Store(Vec<u8>),
    /// A GET, PUT or UPDATE of this room contract.
    Contract(ContractInstanceId),
}

struct Task {
    id: u64,
    kind: Kind,
    step: Step,
    phase: Phase,
    out: Option<(Out, Instant)>,
    /// Connections dropped for this task's unanswered requests.
    reconnects: u32,
    /// Conflicts in the current write.
    conflicts: u32,
    failed: Option<(String, bool)>,
    /// Whether the stored rooms were asked to be read again for this task's finishing.
    reload_asked: bool,
    /// Whether this task's own store put the room in the store. A join that finds the room
    /// already stored under this identity by someone else (a browser tab that accepted the same
    /// invitation) sends nothing: that tab has joined.
    stored_by_us: bool,
}

impl Task {
    fn new(id: u64, kind: Kind, step: Step) -> Task {
        Task {
            id,
            kind,
            step,
            phase: Phase::Read,
            out: None,
            reconnects: 0,
            conflicts: 0,
            failed: None,
            reload_asked: false,
            stored_by_us: false,
        }
    }

    fn owner(&self) -> Option<VerifyingKey> {
        match &self.kind {
            Kind::Create(plan) => Some(plan.room.owner_vk),
            Kind::Join { invitation, .. } => Some(invitation.room),
            Kind::Refused(_) => None,
        }
    }

    /// The room being stored, once there is one.
    fn room(&self) -> Option<&NewRoom> {
        match &self.kind {
            Kind::Create(plan) => Some(&plan.room),
            Kind::Join { plan, .. } => plan.as_ref().map(|p| &p.room),
            Kind::Refused(_) => None,
        }
    }

    fn view(&self) -> WorkView {
        let what = match &self.kind {
            Kind::Create(plan) => format!("Creating “{}”", plan.name()),
            Kind::Join { plan: Some(p), .. } => format!(
                "Joining “{}”",
                super::room::LiveRoom::new(stored(&p.room)).name()
            ),
            Kind::Join { .. } => "Joining a room".to_owned(),
            Kind::Refused(what) => what.clone(),
        };
        let state = match (&self.failed, self.step, self.owner()) {
            (Some((why, retry)), _, _) => WorkState::Failed {
                why: why.clone(),
                retry: *retry,
            },
            (None, Step::Done, Some(owner)) => WorkState::Done {
                room: bs58::encode(owner.to_bytes()).into_string(),
            },
            _ => WorkState::Working,
        };
        WorkView {
            id: self.id,
            what,
            state,
        }
    }

    fn fail(&mut self, why: impl Into<String>, retry: bool) {
        self.out = None;
        self.phase = Phase::Read;
        self.failed = Some((why.into(), retry));
    }

    /// On to the next step.
    fn advance(&mut self) {
        self.out = None;
        self.phase = Phase::Read;
        self.conflicts = 0;
        self.step = match (&self.kind, self.step) {
            (Kind::Create(_), Step::Slot) => Step::Put,
            (Kind::Create(_), Step::Put) => Step::Meta,
            (Kind::Join { .. }, Step::Get) => Step::Slot,
            // Nothing to send (already a member), or another writer stored and joined.
            (Kind::Join { plan, .. }, Step::Slot)
                if !self.stored_by_us || plan.as_ref().is_none_or(|p| p.delta.is_none()) =>
            {
                Step::Meta
            }
            (Kind::Join { .. }, Step::Slot) => Step::Update,
            (Kind::Join { .. }, Step::Update) => Step::Meta,
            (_, Step::Meta) => Step::Done,
            (_, step) => step,
        };
    }

    fn running(&self) -> bool {
        self.failed.is_none() && self.step != Step::Done
    }

    /// What the write step stores, given the key's current value: bytes, nothing (the step is
    /// done), or why it can't. A refusal here does not change by trying again, so it is final.
    fn decide(&mut self, value: Option<&[u8]>) -> Result<Option<Vec<u8>>, String> {
        let unexpected = || "Something unexpected went wrong.".to_owned();
        let owner = self.owner().ok_or_else(unexpected)?;
        if self.step == Step::Meta {
            return meta_with_current_room(value, &owner).map_err(|e| {
                format!(
                    "The room is saved and shows here, but River's room list in its storage \
                     can't be read by this app ({e}), so River in your browser won't open the \
                     room first. Nothing else is affected; you can dismiss this."
                )
            });
        }
        let room = self.room().ok_or_else(unexpected)?;
        // Joining may overwrite a tombstone: accepting an invitation is an explicit rejoin.
        let over_tombstone = matches!(self.kind, Kind::Join { .. });
        match reconcile_new_room(value, room, over_tombstone) {
            NewRoomWrite::Store(bytes) => Ok(Some(bytes)),
            NewRoomWrite::AlreadyStored => {
                // This task's own earlier store, whose answer was lost, holds exactly its bytes.
                if value.is_some() && encode_new_room(room).ok().as_deref() == value {
                    self.stored_by_us = true;
                }
                Ok(None)
            }
            NewRoomWrite::Refuse(why) => Err(why),
        }
    }
}

/// The tasks, oldest first. Only the oldest one still working works.
#[derive(Default)]
pub struct Work {
    tasks: VecDeque<Task>,
    next_id: u64,
    /// A task finished since the stored rooms were last read: read them now.
    reload: bool,
}

impl Work {
    fn push(&mut self, kind: Kind, step: Step) {
        self.next_id += 1;
        self.tasks.push_back(Task::new(self.next_id, kind, step));
    }

    /// A task refused before it started: shown as failed, with nothing to try again.
    fn refuse(&mut self, what: &str, why: String) {
        self.next_id += 1;
        let mut t = Task::new(self.next_id, Kind::Refused(what.to_owned()), Step::Get);
        t.fail(why, false);
        self.tasks.push_back(t);
    }

    /// Create a room. Refused at once (and shown as failed) if its name or nickname can't be
    /// used.
    pub fn create(&mut self, name: &str, nickname: &str, private: bool) {
        let owner = ed25519_dalek::SigningKey::from_bytes(&rand::random::<[u8; 32]>());
        match create::new_room(owner, name, nickname, private) {
            Ok(plan) => self.push(Kind::Create(Box::new(plan)), Step::Slot),
            Err(why) => self.refuse(&format!("Creating “{}”", name.trim()), why),
        }
    }

    /// Join the room `pasted` invites to, as `nickname`. `shown` says whether a room is in the
    /// list already.
    pub fn join(&mut self, pasted: &str, nickname: &str, shown: &dyn Fn(&VerifyingKey) -> bool) {
        const WHAT: &str = "Joining a room";
        let code = invitation_text_from_paste(pasted);
        if code.is_empty() {
            return self.refuse(WHAT, "Paste an invite code.".into());
        }
        let invitation = match Invitation::from_encoded_string(&code) {
            Ok(i) => i,
            Err(e) => {
                return self.refuse(
                    WHAT,
                    format!("That doesn't look like a valid invite code: {e}"),
                );
            }
        };
        if let Some(why) =
            create::join_refusal(nickname).or_else(|| create::invitation_refusal(&invitation))
        {
            return self.refuse(WHAT, why);
        }
        if shown(&invitation.room) {
            // Joining writes the room into the store, and over a stored room only the web UI
            // can write without losing what it keeps there.
            return self.refuse(
                WHAT,
                "You already have this room. To use this invitation instead, accept it in River \
                 in your browser."
                    .into(),
            );
        }
        // One working or just finished: a second join would send a second join event.
        if self
            .tasks
            .iter()
            .any(|t| t.owner() == Some(invitation.room) && (t.running() || t.step == Step::Done))
        {
            return self.refuse(WHAT, "You're already joining this room.".into());
        }
        self.push(
            Kind::Join {
                invitation: Box::new(invitation),
                nickname: nickname.trim().to_owned(),
                plan: None,
            },
            Step::Get,
        );
    }

    /// Try a failed task again from the step it failed at.
    pub fn retry(&mut self, id: u64) -> bool {
        let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        if !matches!(t.failed, Some((_, true))) {
            return false;
        }
        t.failed = None;
        t.reconnects = 0;
        t.conflicts = 0;
        t.out = None;
        t.phase = Phase::Read;
        // A join not yet sent reads the room again, so what it sends is built against the room
        // as it is now, with the time it is sent.
        if let Kind::Join { plan, .. } = &mut t.kind
            && matches!(t.step, Step::Get | Step::Slot | Step::Update)
        {
            *plan = None;
            t.step = Step::Get;
        }
        true
    }

    /// Drop a failed or finished task. A working one stays: its request may be out.
    pub fn dismiss(&mut self, id: u64) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|t| t.id != id || t.running());
        self.tasks.len() != before
    }

    fn active(&mut self) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.running())
    }

    fn active_ref(&self) -> Option<&Task> {
        self.tasks.iter().find(|t| t.running())
    }

    /// Whether a task is working: the stored rooms are not read meanwhile.
    pub fn busy(&self) -> bool {
        self.active_ref().is_some()
    }

    /// Whether the working task has a request out.
    pub fn has_out(&self) -> bool {
        self.active_ref().is_some_and(|t| t.out.is_some())
    }

    /// Whether a task is working on the room owned by `owner`, so nothing else may be sent to
    /// it: an answer names only the room, and the task's must not be taken for another's.
    pub fn holds(&self, owner: &VerifyingKey) -> bool {
        self.active_ref().and_then(Task::owner) == Some(*owner)
    }

    /// Whether to read the stored rooms now, because a task finished. Asking clears it.
    pub fn take_reload(&mut self) -> bool {
        std::mem::take(&mut self.reload)
    }

    /// The connection is gone, and with it whatever was out. Each step starts again from its
    /// beginning on the next one.
    pub fn connection_lost(&mut self) {
        for t in &mut self.tasks {
            t.out = None;
            t.phase = Phase::Read;
        }
    }

    /// The working task's next request, if it has none out. `reading` says the stored rooms
    /// are being read: no task request goes out until that is done.
    pub fn next_request(&mut self, reading: bool, now: Instant) -> Option<ClientRequest<'static>> {
        if reading {
            return None;
        }
        let t = self.active()?;
        if t.out.is_some() {
            return None;
        }
        let owner = t.owner()?;
        let (out, request) = match (t.step, &t.kind) {
            (Step::Get, _) => {
                let id = *create::contract_key(&owner).id();
                (
                    Out::Contract(id),
                    ClientRequest::ContractOp(ContractRequest::Get {
                        key: id,
                        return_contract_code: false,
                        subscribe: false,
                        blocking_subscribe: false,
                    }),
                )
            }
            (Step::Slot | Step::Meta, _) => {
                let key = if t.step == Step::Slot {
                    room_storage_key(&owner)
                } else {
                    ROOMS_META_KEY.to_vec()
                };
                match &t.phase {
                    Phase::Read => (
                        Out::Read(key.clone()),
                        delegate::versioned(&delegate::Versioned::Read(key)),
                    ),
                    Phase::Store { value, generation } => (
                        Out::Store(key.clone()),
                        delegate::versioned(&delegate::Versioned::Store {
                            key,
                            value: value.clone(),
                            generation: *generation,
                        }),
                    ),
                }
            }
            (Step::Put, Kind::Create(plan)) => (
                Out::Contract(*plan.room.contract_key.id()),
                ClientRequest::ContractOp(plan.put()),
            ),
            (Step::Update, Kind::Join { plan: Some(p), .. }) => {
                let Some(delta) = &p.delta else {
                    t.advance();
                    return None;
                };
                let mut bytes = Vec::new();
                if let Err(e) = ciborium::ser::into_writer(delta, &mut bytes) {
                    t.fail(format!("The join could not be encoded: {e}"), false);
                    return None;
                }
                (
                    Out::Contract(*p.room.contract_key.id()),
                    ClientRequest::ContractOp(ContractRequest::Update {
                        key: p.room.contract_key,
                        data: UpdateData::Delta(StateDelta::from(bytes)),
                    }),
                )
            }
            _ => return None,
        };
        t.out = Some((out, now));
        Some(request)
    }

    /// A chat delegate answer. True when it was the working task's.
    pub fn on_delegate(&mut self, answer: &ChatDelegateResponseMsg) -> bool {
        let Some(t) = self.active() else {
            return false;
        };
        let Some((out, _)) = t.out.clone() else {
            return false;
        };
        match (&out, answer) {
            (
                Out::Read(key),
                ChatDelegateResponseMsg::GetVersionedResponse {
                    key: k,
                    value,
                    generation,
                },
            ) if k.as_bytes() == key.as_slice() => match t.decide(value.as_deref()) {
                Ok(Some(bytes)) => {
                    t.out = None;
                    t.phase = Phase::Store {
                        value: bytes,
                        generation: *generation,
                    };
                }
                // Nothing to store: the room is already there, or the meta already opens it.
                Ok(None) => t.advance(),
                Err(why) => t.fail(why, false),
            },
            (Out::Store(key), ChatDelegateResponseMsg::CasStoreResponse { key: k, result })
                if k.as_bytes() == key.as_slice() =>
            {
                match result {
                    CasStoreResult::Stored { .. } => {
                        if t.step == Step::Slot {
                            t.stored_by_us = true;
                        }
                        t.advance()
                    }
                    // Another writer stored first: read what it stored and work from that.
                    CasStoreResult::Conflict { .. } => {
                        t.out = None;
                        t.phase = Phase::Read;
                        t.conflicts += 1;
                        if t.conflicts >= CAS_ATTEMPTS {
                            t.fail(
                                "River's storage kept changing while the room was being saved.",
                                true,
                            );
                        }
                    }
                    CasStoreResult::Failed(e) => t.fail(
                        format!("River's storage on this computer refused: {e}"),
                        true,
                    ),
                }
            }
            _ => return false,
        }
        self.note_done();
        true
    }

    /// A contract answer. True when it was the working task's.
    pub fn on_contract(&mut self, response: &ContractResponse) -> bool {
        let Some(t) = self.active() else {
            return false;
        };
        let Some((Out::Contract(id), _)) = &t.out else {
            return false;
        };
        let id = *id;
        match (t.step, response) {
            (Step::Get, ContractResponse::GetResponse { key, state, .. }) if *key.id() == id => {
                match ciborium::de::from_reader::<ChatRoomStateV1, _>(state.as_ref()) {
                    Ok(state) => {
                        let Kind::Join {
                            invitation,
                            nickname,
                            plan,
                        } = &mut t.kind
                        else {
                            return false;
                        };
                        let key = create::contract_key(&invitation.room);
                        match create::join(invitation, nickname, &state, key, SystemTime::now()) {
                            Ok(p) => {
                                *plan = Some(Box::new(p));
                                t.advance();
                            }
                            Err(r) => t.fail(r.why, r.retry),
                        }
                    }
                    Err(e) => t.fail(
                        format!("Freenet gave back a room this app can't read ({e})."),
                        true,
                    ),
                }
            }
            (Step::Get, ContractResponse::NotFound { instance_id }) if *instance_id == id => t
                .fail(
                    "Freenet couldn't find this room. It may be gone, or Freenet may not reach \
                     it yet.",
                    true,
                ),
            (Step::Put, ContractResponse::PutResponse { key }) if *key.id() == id => t.advance(),
            (Step::Update, ContractResponse::UpdateResponse { key, .. }) if *key.id() == id => {
                t.advance()
            }
            _ => return false,
        }
        self.note_done();
        true
    }

    /// An error from the node. `alone` says nothing but the working task could have caused an
    /// error that names nothing (no update owed, no subscribe or read out); `subscribing` says
    /// a subscribe of a room contract is out, which a `MissingContract` could answer as well.
    /// True when it was the working task's.
    pub fn on_error(
        &mut self,
        e: &ClientError,
        alone: bool,
        subscribing: &dyn Fn(&ContractInstanceId) -> bool,
    ) -> bool {
        let Some(t) = self.active() else {
            return false;
        };
        let Some((out, _)) = &t.out else {
            return false;
        };
        let short = |cause: &str| -> String { cause.chars().take(200).collect() };
        let why = match (out, e.kind()) {
            (Out::Contract(id), ErrorKind::RequestError(RequestError::ContractError(c))) => match c
            {
                ContractError::Get { key, cause } if key.id() == id && t.step == Step::Get => {
                    format!("Couldn't read the room from Freenet: {}", short(cause))
                }
                ContractError::Put { key, cause } if key.id() == id && t.step == Step::Put => {
                    format!(
                        "The room is saved on this computer, but Freenet didn't take it: {}",
                        short(cause)
                    )
                }
                ContractError::Update { key, cause }
                    if key.id() == id && t.step == Step::Update =>
                {
                    format!("Freenet didn't take your join: {}", short(cause))
                }
                ContractError::MissingContract { key } if key == id && !subscribing(key) => {
                    "Freenet doesn't have this room.".to_owned()
                }
                _ => return false,
            },
            // While a task works, nothing else on the connection asks the chat delegate.
            (
                Out::Read(_) | Out::Store(_),
                ErrorKind::RequestError(RequestError::DelegateError(DelegateError::Missing(_))),
            ) => "River's storage isn't on this computer's Freenet under the version this app \
                  knows, so the room can't be saved. Open River in your browser on this \
                  computer first, or update this app."
                .to_owned(),
            (
                Out::Read(_) | Out::Store(_),
                ErrorKind::RequestError(RequestError::DelegateError(d)),
            ) => format!("River's storage on this computer refused: {d}"),
            (Out::Read(_) | Out::Store(_), ErrorKind::Unhandled { cause }) if alone => {
                format!("River's storage on this computer refused: {}", short(cause))
            }
            (
                _,
                ErrorKind::EmptyRing
                | ErrorKind::PeerNotJoined
                | ErrorKind::Shutdown
                | ErrorKind::OperationError { .. }
                | ErrorKind::FailedOperation,
            ) if alone => e.kind().to_string(),
            _ => return false,
        };
        t.fail(why, true);
        true
    }

    /// Whether the working task's request has gone unanswered past its deadline, so the
    /// connection must be dropped: its answer must not arrive late and be taken for a later
    /// request's. The task goes on on the next connection, or fails once it has cost
    /// [`MAX_RECONNECTS`] of them.
    pub fn expire(
        &mut self,
        delegate_wait: Duration,
        network_wait: Duration,
        now: Instant,
    ) -> bool {
        let Some(t) = self.active() else {
            return false;
        };
        let Some((out, at)) = &t.out else {
            return false;
        };
        let wait = match out {
            Out::Read(_) | Out::Store(_) => delegate_wait,
            Out::Contract(_) => network_wait,
        };
        if now.saturating_duration_since(*at) < wait {
            return false;
        }
        t.out = None;
        t.phase = Phase::Read;
        t.reconnects += 1;
        if t.reconnects > MAX_RECONNECTS {
            t.fail("Freenet didn't answer.", true);
        }
        true
    }

    /// A task that just finished asks for the stored rooms to be read, once.
    fn note_done(&mut self) {
        for t in &mut self.tasks {
            if t.step == Step::Done && !t.reload_asked {
                t.reload_asked = true;
                self.reload = true;
            }
        }
    }

    pub fn views(&self) -> Vec<WorkView> {
        self.tasks.iter().map(Task::view).collect()
    }

    /// Drop finished tasks whose room is shown.
    pub fn forget_done(&mut self, shown: &dyn Fn(&VerifyingKey) -> bool) {
        self.tasks.retain(|t| {
            !(t.step == Step::Done && t.failed.is_none() && t.owner().is_some_and(|o| shown(&o)))
        });
    }
}

#[cfg(test)]
#[path = "work_tests.rs"]
mod tests;
