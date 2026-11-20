//! Creating a room, joining one from an invitation and leaving one: the steps each takes on the
//! node, one request at a time, and the only writes this app makes to the chat delegate's
//! storage.
//!
//! **Leaving** ([`super::leave`]) is two writes with no network request: the room's key gets
//! the web UI's tombstone (replacing the room, which holds the user's signing key for it), then
//! `rooms_meta` drops the room from its order, identity sources and open room. The room is
//! hidden from the screen as soon as the task starts and for as long as the tombstone is, or
//! is about to be, stored; a task that fails before the tombstone is stored shows the room
//! again, with Try again. Nothing is published to the room's contract, as in the web UI.
//! Taking the room out of the follow list needs no step: the client API has no unsubscribe,
//! and a room that is no longer listed is no longer shown whatever arrives for it.
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
//! its open room, or one room's notification mode ([`super::modes`]), changed (or, leaving, the
//! entries naming the room taken out), with every other field kept. A conflict never
//! overwrites: what the other writer stored is read and worked from. The one deliberate
//! replacement of a stored room is leaving it, which is what the user asked for.
//!
//! **Rooms from an earlier River need no guard here.** On a node that also holds rooms in an
//! older chat delegate (a re-key the web UI has not carried forward yet), a room this app
//! stores does not hide them from the web UI. Since freenet/river#763 the web UI starts
//! freenet-migrate's completeness journal on every load from the current delegate's room list
//! (`start_journal_load`, `ui/src/components/app/freenet_api/migration_journal.rs`). With no
//! journal stored, `begin_migration_journal` (freenet-migrate 0.8 `delegate_migrate.rs`)
//! starts one with every predecessor pending, whatever the current delegate holds, and an
//! incomplete journal runs the journaled walk (`walk_decision`; the rate limit applies only
//! to predecessors found not installed). The walk imports each older delegate's rooms through
//! `Rooms::merge_from_source` at that generation's rank, below the current delegate's, so a
//! room stored here keeps its identity and a tombstone keeps a room left
//! (`apply_recovered_rooms` in `delegate_migration.rs`). The comment in `chat_delegate.rs`
//! saying legacy migration is skipped when the current delegate has data describes the older
//! sweep and predates #763.
//!
//! Two limits on that:
//! - **Version skew.** A desktop build older than the node's web UI carries an older chat
//!   delegate, registers it, and writes there. The walk reads only predecessors, and after the
//!   journal resolves the newest one it re-reads that one only for its top-up window (14 days
//!   or 30 loads, `TOPUP_WINDOW_MS` / `TOPUP_WINDOW_LOADS` in `migration_journal.rs`). A room
//!   such a build creates after that window never reaches the web UI. Before the app
//!   registered, that build showed "update this app" instead. Detecting a newer delegate
//!   through River's pointer records and refusing to write closes this (`generation.rs`,
//!   freenet/river#888). A desktop-only user's rooms still need carrying forward (#889).
//! - **Schema skew.** The walk runs only over a complete current room set
//!   (`room_set_complete`), which needs every stored slot and `rooms_meta` to decode. A slot
//!   or meta written by this app in a shape an older web UI cannot decode refuses the walk
//!   on every load there. So anything this app stores must stay decodable by the web UI
//!   builds still in use (`river_core::stored_rooms`, pinned to the web UI's types by the
//!   `stored_rooms_drift` tests).
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
//! What this leaves to the web UI: for a room the user owns, asking the delegate to follow it
//! (`EnsureRoomSubscription`), which the web UI does for every stored room each time it loads
//! (`hydrate_loaded_rooms_with_authority` in `freenet_api/response_handler.rs`). The room's
//! signing key is stored in the delegate by the background sweep in [`super::keys`], not by
//! these tasks, so a task is done as soon as the room is stored.

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{
    ClientError, ClientRequest, ContractError, ContractRequest, ContractResponse, DelegateError,
    ErrorKind, RequestError,
};
use freenet_stdlib::prelude::{ContractInstanceId, StateDelta, UpdateData};
use river_core::chat_delegate::{CasStoreResult, ChatDelegateResponseMsg};
use river_core::invitation::Invitation;
use river_core::room_state::ChatRoomStateV1;
use river_core::room_state::identity::IdentityExport;
use river_core::stored_rooms::{
    NewRoom, NewRoomWrite, ROOMS_META_KEY, StoredRoom, StoredRoomSlot, decode_room_slot,
    encode_new_room, meta_with_current_room, reconcile_new_room, room_storage_key,
};

use super::create::{self, JoinPlan, NewRoomPlan};
use super::delegate;
use super::dm_cache;
use super::dm_store::{self, DmOp, EntryKey};
use super::identity::{self, IdentityToken, ImportWrite};
use super::leave::{meta_without_room, tombstone_write};
use super::modes::{NotificationMode, meta_with_mode};

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

/// An import that was stored. No `Debug`: the nickname is private.
pub struct ImportDone {
    /// Whether the room was read from the network for this import (an add), so the state it
    /// was stored with is current.
    pub fetched: bool,
    pub room: VerifyingKey,
    pub identity: VerifyingKey,
    pub nickname: Option<String>,
}

/// A task as the screen shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkView {
    pub id: u64,
    /// What it is, for the screen: "Creating “Book club”", "Joining a room".
    pub what: String,
    /// The room a join is for (its key), from the start, so a card that started a join can tell
    /// its own from another. `None` for everything else.
    pub room: Option<String>,
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

/// How a sent-message write ended ([`Work::take_dm_ends`]).
#[derive(Debug, PartialEq, Eq)]
pub struct DmEnd {
    /// The store holds what the write asked for.
    pub ok: bool,
    /// The entries it was to add.
    pub added: Vec<EntryKey>,
    /// The entries it was to remove.
    pub removed: Vec<EntryKey>,
    /// Entries of the user it dropped to stay in bounds.
    pub evicted: Vec<EntryKey>,
    /// Why the merge refused, if it did.
    pub refusal: Option<dm_store::Refusal>,
}

enum Kind {
    Create(Box<NewRoomPlan>),
    Join {
        invitation: Box<Invitation>,
        nickname: String,
        /// The join must add a new member: it is refused when the invitee is a member of the
        /// room already. For an invitation from a message, which could hand over the key of a
        /// member of that room (an identity to adopt).
        only_new: bool,
        /// Built when the room is read, so its join event carries that time. Kept across
        /// connections, so a resend is the same bytes; built again on Try again, so a join
        /// retried much later is not stamped with a time the room has trimmed.
        plan: Option<Box<JoinPlan>>,
    },
    /// Store `mode` as `room`'s notification mode in `rooms_meta` (the one step, `Meta`).
    /// Not about a room being created or joined: it has no owner as far as the rest of this
    /// module is concerned (so it holds no room), and is not shown unless it fails.
    Mode {
        room: VerifyingKey,
        mode: NotificationMode,
    },
    /// Leave `room` (named `name` on the screen): tombstone its key (the step `Slot`), then take
    /// it out of `rooms_meta` (`Meta`), as the web UI's leave does ([`super::leave`]).
    /// `identity` is the user's key in the room when it was left: a stored room under another
    /// one was joined again since, and a retry must not tombstone it.
    Leave {
        room: VerifyingKey,
        name: String,
        identity: Option<Box<VerifyingKey>>,
    },
    /// Import an identity (`export`, checked already): add its room, or with `replace` swap the
    /// identity of the room the store holds ([`super::identity`]). Adding reads the room from the
    /// network first (`plan` holds the room built from it); replacing starts at the slot.
    Import {
        export: Box<IdentityExport>,
        replace: bool,
        plan: Option<Box<NewRoom>>,
    },
    /// Change the sent-message store (`outbound_dms`) by `ops`, merged into what it holds now
    /// ([`super::dm_store`]). One step, `Meta`, on a different key. Never shown, not even when
    /// it fails: the session retries it on its own ([`super::dm_save`]). `listed` says the
    /// cache had read the key with a value.
    DmSave {
        ops: Vec<DmOp>,
        listed: bool,
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
#[derive(Clone, PartialEq, Eq)]
enum Phase {
    /// Read the key and its generation.
    Read,
    /// Store these bytes if the key is still at this generation.
    Store { value: Vec<u8>, generation: u64 },
}

/// The bytes of a store can be what the user wrote, so only their length is printed.
impl std::fmt::Debug for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Phase::Read => f.write_str("Read"),
            Phase::Store { value, generation } => f
                .debug_struct("Store")
                .field("value", &format_args!("<{} bytes>", value.len()))
                .field("generation", generation)
                .finish(),
        }
    }
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

/// A wait before the next request, for a write that was beaten to the key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Cool {
    #[default]
    No,
    /// Work out when, at the next request.
    Wanted,
    Until(Instant),
}

/// The first wait after a write lost the compare-and-swap, and the most it doubles to. Another
/// writer got in first, so asking again at once mostly meets the same writer.
const CONFLICT_WAIT_FIRST: Duration = Duration::from_millis(25);
const CONFLICT_WAIT_MAX: Duration = Duration::from_millis(400);

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
    /// For a mode task: whether its end (stored, or failed) was handed to [`Work::settle_modes`].
    settled: bool,
    /// Whether the node answered the last request with a refusal of it (the UPDATE was not
    /// taken, or the room is not there), rather than leaving its outcome unknown.
    refused: bool,
    /// For a [`Kind::DmSave`]: the wait before it reads again after a conflict.
    cool: Cool,
    /// For a [`Kind::DmSave`] that ended well: the entries it dropped to stay in bounds.
    dm_evicted: Vec<EntryKey>,
    /// For a [`Kind::DmSave`]: the adds it left out because their room is not shown.
    dm_dropped: Vec<EntryKey>,
    /// For a [`Kind::DmSave`]: the rooms it may add for, as of the answer being decided.
    dm_rooms: Vec<[u8; 32]>,
    /// For a [`Kind::DmSave`] the merge refused: why.
    dm_refusal: Option<dm_store::Refusal>,
    /// For a finished join: the room's name, kept for the line that says it is done after the
    /// plan (which holds the invitee's key and the room's secrets) has been wiped.
    done_name: Option<String>,
}

/// For tests: how many non-zero secrets of an invitation and of a join plan were wiped on this
/// thread, so a test can see that a copy was reached (a dropped value cannot be looked at).
#[cfg(test)]
pub(crate) mod wiped {
    use std::cell::Cell;

    thread_local! {
        static COUNTS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    }

    pub(super) fn note(invitation: bool, secret: &[u8; 32]) {
        if *secret != [0; 32] {
            COUNTS.with(|c| {
                let (i, p) = c.get();
                c.set(if invitation { (i + 1, p) } else { (i, p + 1) });
            });
        }
    }

    /// (invitation secrets, plan secrets) wiped since the last call, and reset.
    pub(crate) fn take() -> (usize, usize) {
        COUNTS.with(|c| c.replace((0, 0)))
    }
}

impl Kind {
    /// Wipe the credential a join holds: the invitation's secrets and signing key, and the plan
    /// (which holds the invitee's key as the room's `self_sk`, and the secrets). The room it is
    /// for stays, so the task still names it.
    fn wipe_credential(&mut self) {
        self.wipe_invitation();
        if let Kind::Join { plan, .. } = self {
            if let Some(p) = plan.as_mut() {
                for secret in p.room.invitation_secrets.values_mut() {
                    #[cfg(test)]
                    wiped::note(false, secret);
                    zeroize::Zeroize::zeroize(secret);
                }
            }
            *plan = None;
        }
    }

    /// Wipe the invitation of a join: its room secrets, and its signing key (replaced by a zero
    /// one; the old key wipes itself as it is dropped).
    fn wipe_invitation(&mut self) {
        if let Kind::Join { invitation, .. } = self {
            for (_, secret) in &mut invitation.room_secrets {
                #[cfg(test)]
                wiped::note(true, secret);
                zeroize::Zeroize::zeroize(secret);
            }
            invitation.room_secrets.clear();
            invitation.invitee_signing_key = ed25519_dalek::SigningKey::from_bytes(&[0; 32]);
        }
    }
}

/// A task that held an invitation wipes it when it goes (finished, put away, or the session
/// ended): an `Invitation` does not wipe its room secrets itself.
impl Drop for Task {
    fn drop(&mut self) {
        self.kind.wipe_credential();
    }
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
            settled: false,
            refused: false,
            cool: Cool::No,
            dm_evicted: Vec::new(),
            dm_dropped: Vec::new(),
            dm_rooms: Vec::new(),
            dm_refusal: None,
            done_name: None,
        }
    }

    fn owner(&self) -> Option<VerifyingKey> {
        match &self.kind {
            Kind::Create(plan) => Some(plan.room.owner_vk),
            Kind::Join { invitation, .. } => Some(invitation.room),
            Kind::Leave { room, .. } => Some(*room),
            Kind::Import { export, .. } => Some(export.room_owner),
            Kind::Mode { .. } | Kind::DmSave { .. } | Kind::Refused(_) => None,
        }
    }

    /// Whether this only saves a notification mode.
    fn is_mode(&self) -> bool {
        matches!(self.kind, Kind::Mode { .. })
    }

    /// Whether this changes the sent-message store.
    fn is_dm(&self) -> bool {
        matches!(self.kind, Kind::DmSave { .. })
    }

    /// Whether this leaves a room.
    fn is_leave(&self) -> bool {
        matches!(self.kind, Kind::Leave { .. })
    }

    /// The room being stored, once there is one.
    fn room(&self) -> Option<&NewRoom> {
        match &self.kind {
            Kind::Create(plan) => Some(&plan.room),
            Kind::Join { plan, .. } => plan.as_ref().map(|p| &p.room),
            Kind::Leave { .. } | Kind::Mode { .. } | Kind::DmSave { .. } | Kind::Refused(_) => None,
            Kind::Import { plan, .. } => plan.as_deref(),
        }
    }

    fn view(&self) -> WorkView {
        let what = match &self.kind {
            Kind::Create(plan) => format!("Creating “{}”", plan.name()),
            Kind::Join { .. } if self.done_name.is_some() => format!(
                "Joining “{}”",
                self.done_name.as_deref().unwrap_or_default()
            ),
            Kind::Join { plan: Some(p), .. } => format!(
                "Joining “{}”",
                super::room::LiveRoom::new(stored(&p.room)).name()
            ),
            Kind::Join { .. } => "Joining a room".to_owned(),
            Kind::Import { replace: true, .. } => "Replacing your ID in a room".to_owned(),
            Kind::Import { .. } => "Importing your ID".to_owned(),
            Kind::Mode { .. } => "Saving the notification setting".to_owned(),
            Kind::DmSave { .. } => "Saving sent messages".to_owned(),
            Kind::Leave { name, .. } => format!("Leaving “{name}”"),
            Kind::Refused(what) => what.clone(),
        };
        let state = match (&self.failed, self.step, self.owner()) {
            (Some((why, retry)), _, _) => WorkState::Failed {
                why: if self.is_leave() {
                    format!("{why} Unsent messages for this room were discarded.")
                } else {
                    why.clone()
                },
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
            room: self
                .is_join()
                .then(|| self.owner())
                .flatten()
                .map(|o| bs58::encode(o.to_bytes()).into_string()),
            state,
        }
    }

    /// Forget the bytes a write was about to store: for a sent-message store they hold the
    /// text of what the user wrote.
    fn wipe(&mut self) {
        if let Phase::Store { value, .. } = &mut self.phase {
            zeroize::Zeroize::zeroize(value);
        }
        self.phase = Phase::Read;
    }

    fn fail(&mut self, why: impl Into<String>, retry: bool) {
        self.out = None;
        self.wipe();
        // A join that can't be tried again has no use for the invitation it holds.
        if !retry {
            self.kind.wipe_credential();
        }
        self.failed = Some((why.into(), retry));
    }

    fn is_join(&self) -> bool {
        matches!(self.kind, Kind::Join { .. })
    }

    /// On to the next step.
    fn advance(&mut self) {
        self.out = None;
        self.wipe();
        self.conflicts = 0;
        self.step = match (&self.kind, self.step) {
            (Kind::Create(_), Step::Slot) => Step::Put,
            (Kind::Create(_), Step::Put) => Step::Meta,
            (Kind::Join { .. } | Kind::Import { .. }, Step::Get) => Step::Slot,
            // Nothing to send: the room, read just before, already has the invitee as a member
            // (another tab that accepted the same invitation, or an invitation accepted twice).
            // That is decided from the room alone, as the web UI's `build_state_for_put` does,
            // never from who stored the slot.
            (Kind::Join { plan, .. }, Step::Slot)
                if plan.as_ref().is_none_or(|p| p.delta.is_none()) =>
            {
                Step::Meta
            }
            (Kind::Join { .. }, Step::Slot) => Step::Update,
            (Kind::Join { .. }, Step::Update) => Step::Meta,
            (Kind::Leave { .. }, Step::Slot) => Step::Meta,
            (Kind::Import { .. }, Step::Slot) => Step::Meta,
            (_, Step::Meta) => Step::Done,
            (_, step) => step,
        };
        // A finished join has no use for the invitation or the plan: nothing reads them after the
        // last write but the line that names the room, which keeps the name.
        if self.step == Step::Done {
            if let Kind::Join { plan: Some(p), .. } = &self.kind {
                self.done_name = Some(super::room::LiveRoom::new(stored(&p.room)).name());
            }
            self.kind.wipe_credential();
        }
    }

    fn running(&self) -> bool {
        self.failed.is_none() && self.step != Step::Done
    }

    /// What the write step stores, given the key's current value: bytes, nothing (the step is
    /// done), or why it can't. A refusal here does not change by trying again, so it is final.
    fn decide(&mut self, value: Option<&[u8]>) -> Result<Option<Vec<u8>>, String> {
        let unexpected = || "Something unexpected went wrong.".to_owned();
        if let Kind::DmSave { ops, listed } = &self.kind {
            // An add for a room that is not shown (left meanwhile) is not written: its entry
            // would be orphaned. It is only left out of this write: its text stays in the cache
            // until a complete read of the rooms says the room is gone, so a room missing from a
            // partial list is saved when it is back.
            let rooms = &self.dm_rooms;
            let (kept, dropped): (Vec<DmOp>, Vec<DmOp>) =
                ops.iter().cloned().partition(|op| match op {
                    DmOp::Add { key, .. } => rooms.contains(&key.0),
                    DmOp::Remove { .. } => true,
                });
            let dropped: Vec<EntryKey> = dropped
                .iter()
                .filter_map(|op| match op {
                    DmOp::Add { key, .. } => Some(*key),
                    DmOp::Remove { .. } => None,
                })
                .collect();
            let merged = dm_store::merge(value, *listed, &kept);
            self.dm_dropped = dropped;
            return match merged {
                Ok(m) => {
                    self.dm_evicted = m.evicted;
                    self.dm_refusal = None;
                    Ok(m.bytes)
                }
                Err(refusal) => {
                    self.dm_refusal = Some(refusal);
                    Err(refusal.to_string())
                }
            };
        }
        if let Kind::Mode { room, mode } = &self.kind {
            return meta_with_mode(value, room, *mode).map_err(|e| {
                format!(
                    "The notification setting was not saved: River's room list in its storage \
                     can't be read by this app ({e}), so it stays as it was."
                )
            });
        }
        if let Kind::Leave { room, identity, .. } = &self.kind {
            return if self.step == Step::Slot {
                // Joined again since (another invitation, or the web UI), under another key:
                // that room is not the one left, and is left alone.
                if let (Some(mine), Some(bytes)) = (identity, value)
                    && let Ok(StoredRoomSlot::Present(stored)) = decode_room_slot(bytes)
                    && stored.self_verifying_key().is_some_and(|k| k != **mine)
                {
                    return Err("You have joined this room again since, so it was not \
                                left. Nothing was changed."
                        .into());
                }
                tombstone_write(value)
            } else {
                meta_without_room(value, room).map_err(|e| {
                    format!(
                        "You left the room, but River's room list in its storage can't be read \
                         by this app ({e}), so River in your browser may still list it until it \
                         next saves. You can dismiss this."
                    )
                })
            };
        }
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
        if let Kind::Import {
            export,
            replace: true,
            ..
        } = &self.kind
        {
            return match identity::slot_with_identity(value, export)? {
                ImportWrite::Store(bytes) => Ok(Some(bytes)),
                ImportWrite::Unchanged => Ok(None),
                ImportWrite::Refuse(why) => Err(why),
            };
        }
        let room = self.room().ok_or_else(unexpected)?;
        if let Kind::Import { export, .. } = &self.kind {
            return match identity::add_room(value, room, export) {
                ImportWrite::Store(bytes) => Ok(Some(bytes)),
                ImportWrite::Unchanged => Ok(None),
                ImportWrite::Refuse(why) => Err(why),
            };
        }
        // Joining may overwrite a tombstone: accepting an invitation is an explicit rejoin.
        let over_tombstone = matches!(self.kind, Kind::Join { .. });
        match reconcile_new_room(value, room, over_tombstone) {
            NewRoomWrite::Store(bytes) => Ok(Some(bytes)),
            NewRoomWrite::AlreadyStored => Ok(None),
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
    /// The rooms (owner key bytes) the sent-message saves may add entries for: the rooms shown
    /// now. An add for any other room is dropped before it is written.
    dm_rooms: Vec<[u8; 32]>,
    /// The last request that went unanswered was a sent-message save's.
    expired_dm: bool,
    /// A fixed point of the jitter's range (0 to 1) for tests; random otherwise.
    jitter_unit: Option<f64>,
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
        let invitation = match create::decode_invitation(pasted) {
            Ok(i) => i,
            Err(why) => return self.refuse(WHAT, why),
        };
        if let Err(why) = self.join_invitation(invitation, nickname, false, shown) {
            self.refuse(WHAT, why);
        }
    }

    /// Join the room `invitation` is for, as `nickname`, or say at once why it is not queued
    /// (nothing is shown as failed: the caller says it). The invitation is not checked for
    /// consistency here beyond what [`create::join`] does when the room is read.
    pub fn join_invitation(
        &mut self,
        invitation: Invitation,
        nickname: &str,
        only_new: bool,
        shown: &dyn Fn(&VerifyingKey) -> bool,
    ) -> Result<(), String> {
        if let Some(why) = create::join_refusal(nickname) {
            super::dm_invite::wipe_invitation(invitation);
            return Err(why);
        }
        if shown(&invitation.room) {
            // Joining writes the room into the store, and over a stored room only the web UI
            // can write without losing what it keeps there. The window opens such a room
            // instead of asking for this ([`crate::chat::Chat::submit_join`]); this is for an
            // invitation that reaches here some other way.
            super::dm_invite::wipe_invitation(invitation);
            return Err(
                "You are already a member of this room, so this invitation was not used.".into(),
            );
        }
        // Another join of this room, working, finished, or failed with Try again: a second one
        // would send a second join event.
        let earlier = self
            .tasks
            .iter()
            .filter(|t| t.owner() == Some(invitation.room) && !t.is_leave())
            .find_map(|t| {
                if t.step == Step::Done && t.failed.is_none() {
                    Some("You have just joined this room.")
                } else if t.running() {
                    Some("You're already joining this room.")
                } else if matches!(t.failed, Some((_, true))) {
                    Some(
                        "You're already joining this room: use Try again on it, or dismiss it \
                         first.",
                    )
                } else {
                    None
                }
            });
        if let Some(why) = earlier {
            super::dm_invite::wipe_invitation(invitation);
            return Err(why.into());
        }
        // A leave of this room that failed is over: it must not be tried again over the room
        // joined now. So is a join that failed with nothing to try again: it is replaced, not
        // shown beside the new one.
        self.tasks.retain(|t| {
            let over = if t.is_leave() {
                t.failed.is_some()
            } else {
                t.is_join() && matches!(t.failed, Some((_, false)))
            };
            !(over && t.owner() == Some(invitation.room))
        });
        self.push(
            Kind::Join {
                invitation: Box::new(invitation),
                nickname: nickname.trim().to_owned(),
                only_new,
                plan: None,
            },
            Step::Get,
        );
        Ok(())
    }

    /// Leave `room`, which the screen calls `name`. Nothing is queued when it is being left
    /// already. Returns whether anything was.
    pub fn leave(
        &mut self,
        room: VerifyingKey,
        name: String,
        identity: Option<VerifyingKey>,
    ) -> bool {
        if self.leaving(&room) {
            return false;
        }
        // An earlier leave that failed is replaced by this one, not shown beside it.
        self.tasks
            .retain(|t| !(t.is_leave() && t.owner() == Some(room) && t.failed.is_some()));
        self.push(
            Kind::Leave {
                room,
                name,
                identity: identity.map(Box::new),
            },
            Step::Slot,
        );
        true
    }

    /// Whether `room` is being left, or was left and the stored rooms have not been read since.
    /// A leave that failed does not count: before its tombstone was stored the room is still
    /// the user's (the stored rooms are read again to show it), and after, a read finds the
    /// tombstone.
    pub fn leaving(&self, room: &VerifyingKey) -> bool {
        self.tasks.iter().any(|t| {
            matches!(&t.kind, Kind::Leave { room: r, .. } if r == room) && t.failed.is_none()
        })
    }

    /// Import the identity `pasted` holds. Without `replace` it adds the room (refused if one
    /// is shown); with it, it swaps the identity of the room the store holds. `shown` says
    /// whether a room is in the list already.
    pub fn import_identity(
        &mut self,
        pasted: &IdentityToken,
        replace: bool,
        shown: &dyn Fn(&VerifyingKey) -> bool,
    ) {
        const WHAT: &str = "Importing your ID";
        let export = match identity::parse_token(&pasted.0) {
            Ok(e) => e,
            Err(why) => return self.refuse(WHAT, why),
        };
        let owner = export.room_owner;
        if shown(&owner) && !replace {
            return self.refuse(
                WHAT,
                "You already have this room. Import again and choose to replace your identity in it."
                    .into(),
            );
        }
        // Another create, join or import of this room, working or waiting for Try again: a
        // second one would race it for the same slot.
        // A leave of the room is not "another change" here: tasks run one at a time, so an import
        // queued behind a leave runs after it and finds its tombstone (an explicit rejoin).
        let earlier = self
            .tasks
            .iter()
            .filter(|t| t.owner() == Some(owner) && !t.is_leave())
            .find_map(|t| {
            if t.running() {
                Some("This room is being changed: wait for that to finish first.")
            } else if matches!(t.failed, Some((_, true))) {
                Some("This room has a change that failed: use Try again on it, or dismiss it first.")
            } else {
                None
            }
        });
        if let Some(why) = earlier {
            return self.refuse(WHAT, why.into());
        }
        // A leave of this room that failed is over when the room is added again: it must not be
        // tried again over it. A replace keeps the row: the user still meant to leave, and a retry
        // does not tombstone a room whose identity changed since.
        if !replace {
            self.tasks
                .retain(|t| !(t.is_leave() && t.owner() == Some(owner) && t.failed.is_some()));
        }
        let step = if replace { Step::Slot } else { Step::Get };
        self.push(
            Kind::Import {
                export: Box::new(export),
                replace,
                plan: None,
            },
            step,
        );
    }

    /// Store `mode` as `room`'s notification mode. A save still waiting to start for the same
    /// room is changed instead of queueing another, so picking quickly writes the last pick.
    pub fn set_mode(&mut self, room: VerifyingKey, mode: NotificationMode) {
        if let Some(Task {
            kind: Kind::Mode { mode: queued, .. },
            ..
        }) = self.tasks.iter_mut().rev().find(|t| {
            matches!(&t.kind, Kind::Mode { room: r, .. } if *r == room)
                && t.running()
                && t.out.is_none()
                && t.phase == Phase::Read
        }) {
            *queued = mode;
            return;
        }
        self.push(Kind::Mode { room, mode }, Step::Meta);
        // Nothing here changes what the stored rooms say beyond what the screen already shows.
        if let Some(t) = self.tasks.back_mut() {
            t.reload_asked = true;
        }
    }

    /// The modes chosen and still being stored: they override what the store says, which is
    /// older. One that failed is not here: it was not saved, so the store's word stands.
    pub fn pending_modes(&self) -> Vec<(VerifyingKey, NotificationMode)> {
        self.tasks
            .iter()
            .filter(|t| t.running())
            .filter_map(|t| match &t.kind {
                Kind::Mode { room, mode } => Some((*room, *mode)),
                _ => None,
            })
            .collect()
    }

    /// The mode saves that ended since the last call: the room, the mode, and whether it was
    /// stored (else it failed and the store still holds what it did). Each is reported once.
    pub fn settle_modes(&mut self) -> Vec<(VerifyingKey, NotificationMode, bool)> {
        let mut out = Vec::new();
        for t in &mut self.tasks {
            let Kind::Mode { room, mode } = &t.kind else {
                continue;
            };
            if t.settled || t.running() {
                continue;
            }
            t.settled = true;
            out.push((*room, *mode, t.failed.is_none()));
        }
        out
    }

    /// Change the sent-message store by `ops` (one write at a time: nothing is queued while
    /// another is). False when one is already queued or working.
    pub fn dm_save(&mut self, ops: Vec<DmOp>, listed: bool) -> bool {
        if self.has_dm() || ops.is_empty() {
            return false;
        }
        self.push(Kind::DmSave { ops, listed }, Step::Meta);
        // Nothing here changes what the stored rooms say.
        if let Some(t) = self.tasks.back_mut() {
            t.reload_asked = true;
        }
        true
    }

    /// The rooms shown now: a sent-message save adds entries only for these.
    pub fn set_dm_rooms(&mut self, rooms: Vec<[u8; 32]>) {
        self.dm_rooms = rooms;
    }

    /// Ask for the stored rooms to be read again.
    pub fn ask_reload(&mut self) {
        self.reload = true;
    }

    /// Whether the request that last went unanswered was a sent-message save's.
    pub fn expired_dm(&self) -> bool {
        self.expired_dm
    }

    #[cfg(test)]
    fn fix_jitter(&mut self, unit: f64) {
        self.jitter_unit = Some(unit);
    }

    /// Whether a sent-message write is queued or working.
    pub fn has_dm(&self) -> bool {
        self.tasks.iter().any(Task::is_dm)
    }

    /// The sent-message writes that ended since the last call, each once. Their tasks go.
    pub fn take_dm_ends(&mut self) -> Vec<DmEnd> {
        let mut ends = Vec::new();
        self.tasks.retain_mut(|t| {
            if !t.is_dm() || t.running() {
                return true;
            }
            let ok = t.failed.is_none();
            let Kind::DmSave { ops, .. } = &t.kind else {
                return true;
            };
            let (mut added, mut removed) = (Vec::new(), Vec::new());
            for op in ops {
                match op {
                    // An add left out for a room that is gone is not one the store holds.
                    DmOp::Add { key, .. } if t.dm_dropped.contains(key) => {}
                    DmOp::Add { key, .. } => added.push(*key),
                    DmOp::Remove { key } => removed.push(*key),
                }
            }
            ends.push(DmEnd {
                ok,
                added,
                removed,
                evicted: std::mem::take(&mut t.dm_evicted),
                refusal: t.dm_refusal.take(),
            });
            false
        });
        ends
    }

    /// The imports that finished since the last call: the room, the imported identity, and the
    /// nickname the token carries, if any. Each is reported once.
    pub fn take_done_imports(&mut self) -> Vec<ImportDone> {
        let mut out = Vec::new();
        for t in &mut self.tasks {
            let Kind::Import {
                export, replace, ..
            } = &t.kind
            else {
                continue;
            };
            if t.settled || t.step != Step::Done || t.failed.is_some() {
                continue;
            }
            t.settled = true;
            out.push(ImportDone {
                fetched: !*replace,
                room: export.room_owner,
                identity: export.signing_key.verifying_key(),
                nickname: identity::nickname_of(export),
            });
        }
        out
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
        t.settled = false;
        t.reload_asked = t.is_mode();
        t.reconnects = 0;
        t.conflicts = 0;
        t.out = None;
        t.phase = Phase::Read;
        // A join reads the room again, so what it sends is built against the room as it is now,
        // with the time it is sent, when it failed before its UPDATE, or when the node refused
        // the UPDATE (the room may have filled up or banned the invitee since; whether to send
        // at all is decided from the room, so reading it again can't join twice). When the
        // UPDATE's outcome is unknown (no answer, or an error that says nothing about it), the
        // same join goes again: the node may hold it, and the same message id can't show twice.
        let refused = std::mem::take(&mut t.refused);
        if let Kind::Join { plan, .. } = &mut t.kind
            && (matches!(t.step, Step::Get | Step::Slot) || (t.step == Step::Update && refused))
        {
            *plan = None;
            t.step = Step::Get;
        }
        // An import that adds a room reads it again, as a join does. One that replaces an
        // identity has nothing to read: it works out its write from the slot again.
        if let Kind::Import {
            replace: false,
            plan,
            ..
        } = &mut t.kind
            && matches!(t.step, Step::Get | Step::Slot)
        {
            *plan = None;
            t.step = Step::Get;
        }
        true
    }

    /// Fail every task still working with nothing out, with `why` and no Try again: River has
    /// moved on and this build writes nothing more ([`super::generation`]). A task with a
    /// request out keeps it until its answer or its deadline, so that answer is never taken
    /// for another's; it is failed here the next time. True when anything failed.
    pub fn halt(&mut self, why: &str) -> bool {
        let mut changed = false;
        for t in &mut self.tasks {
            if t.running() && t.out.is_none() {
                t.fail(why, false);
                changed = true;
            }
        }
        // A leave halted before its tombstone shows its room again.
        self.note_done();
        changed
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

    /// Whether a read of the stored rooms is wanted because a task finished, without clearing it.
    pub fn reload_pending(&self) -> bool {
        self.reload
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
            t.wipe();
        }
    }

    /// The working task's next request, if it has none out. `reading` says the stored rooms
    /// are being read: no task request goes out until that is done.
    pub fn next_request(&mut self, reading: bool, now: Instant) -> Option<ClientRequest<'static>> {
        if reading {
            return None;
        }
        let unit = self.jitter_unit.unwrap_or_else(rand::random::<f64>);
        let t = self.active()?;
        if t.out.is_some() {
            return None;
        }
        match t.cool {
            Cool::No => {}
            // A write that lost the race waits a little, so the other writer can finish. The
            // wait is spread by plus or minus a fifth, so writers that lost together do not
            // meet again together.
            Cool::Wanted => {
                let wait = CONFLICT_WAIT_FIRST
                    .saturating_mul(1u32 << t.conflicts.saturating_sub(1).min(8))
                    .min(CONFLICT_WAIT_MAX)
                    .mul_f64(0.8 + 0.4 * unit.clamp(0.0, 1.0));
                t.cool = Cool::Until(now + wait);
                return None;
            }
            Cool::Until(at) if now < at => return None,
            Cool::Until(_) => t.cool = Cool::No,
        }
        let owner = t.owner();
        let (out, request) = match (t.step, &t.kind) {
            (Step::Get, _) => {
                let id = *create::contract_key(&owner?).id();
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
                    room_storage_key(&owner?)
                } else if t.is_dm() {
                    dm_cache::KEY.to_vec()
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
        let rooms = self.dm_rooms.clone();
        let Some(t) = self.active() else {
            return false;
        };
        let Some((out, _)) = t.out.clone() else {
            return false;
        };
        if t.is_dm() {
            t.dm_rooms = rooms;
        }
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
                    CasStoreResult::Stored { .. } => t.advance(),
                    // Another writer stored first: read what it stored and work from that.
                    CasStoreResult::Conflict { .. } => {
                        t.out = None;
                        t.wipe();
                        t.conflicts += 1;
                        if t.is_dm() {
                            t.cool = Cool::Wanted;
                        }
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
                    Ok(state) => match &mut t.kind {
                        Kind::Join {
                            invitation,
                            nickname,
                            only_new,
                            plan,
                        } => {
                            let key = create::contract_key(&invitation.room);
                            // A nickname made for the user (a join from a message) is cut to
                            // what this room allows; one the user typed is not changed.
                            let nick = if *only_new {
                                create::fit_nickname(nickname, &state.configuration.configuration)
                            } else {
                                Some(nickname.clone())
                            };
                            let Some(nick) = nick else {
                                t.fail(
                                    "This room's nickname limit is too small for a generated nickname, so the invitation is not used.",
                                    false,
                                );
                                return true;
                            };
                            match create::join(invitation, &nick, &state, key, SystemTime::now()) {
                                Ok(p) => {
                                    // Kept in the task before it is judged, so a plan that is
                                    // refused is wiped with the task's credential.
                                    let rejected = *only_new && !p.adds_member();
                                    *plan = Some(Box::new(p));
                                    if rejected {
                                        t.fail(
                                            "This invitation is for someone who is a member of the room already, so it is not used.",
                                            false,
                                        );
                                    } else {
                                        t.advance();
                                    }
                                }
                                Err(r) => t.fail(r.why, r.retry),
                            }
                        }
                        Kind::Import { export, plan, .. } => {
                            let key = create::contract_key(&export.room_owner);
                            match identity::new_room(export, &state, key) {
                                Ok(room) => {
                                    *plan = Some(Box::new(room));
                                    t.advance();
                                }
                                Err(r) => t.fail(r.why, r.retry),
                            }
                        }
                        _ => return false,
                    },
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
        // An answer about this very request: the node refused it.
        let refused = matches!(
            (out, e.kind()),
            (
                Out::Contract(_),
                ErrorKind::RequestError(RequestError::ContractError(
                    ContractError::Update { .. } | ContractError::MissingContract { .. }
                ))
            )
        );
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
            // The session reconnects on this, which registers the store again. Only the chat
            // delegate's own `Missing` (the session's `chat_delegate_missing`); another key's
            // falls to the arm below.
            (
                Out::Read(_) | Out::Store(_),
                ErrorKind::RequestError(RequestError::DelegateError(DelegateError::Missing(key))),
            ) if key == delegate::chat_delegate_key() => {
                "River's storage went missing from this computer's Freenet, so the room \
                  wasn't saved. River is setting it up again: try again in a moment."
                    .to_owned()
            }
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
        t.refused = refused;
        self.note_done();
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
        t.wipe();
        t.reconnects += 1;
        // A save of sent messages may not cost the session more than one new connection: it is
        // tried again later on its own pace.
        let is_dm = t.is_dm();
        let limit = if is_dm { 1 } else { MAX_RECONNECTS };
        if t.reconnects > limit {
            t.fail("Freenet didn't answer.", true);
            self.note_done();
        }
        self.expired_dm = is_dm;
        true
    }

    /// A task that just finished asks for the stored rooms to be read, once.
    fn note_done(&mut self) {
        for t in &mut self.tasks {
            // A leave that failed before the tombstone was stored: the room was taken off the
            // screen, so the stored rooms are read to show it again.
            let restored = t.is_leave() && t.failed.is_some() && t.step == Step::Slot;
            if (t.step == Step::Done || restored) && !t.reload_asked {
                t.reload_asked = true;
                self.reload = true;
            }
        }
    }

    /// Whether no join task holds an invitation's key or secrets, for tests.
    #[cfg(test)]
    pub(crate) fn join_credentials_wiped(&self) -> bool {
        self.tasks.iter().all(|t| match &t.kind {
            Kind::Join {
                invitation, plan, ..
            } => {
                invitation.room_secrets.is_empty()
                    && invitation.invitee_signing_key.to_bytes() == [0; 32]
                    && plan.is_none()
            }
            _ => true,
        })
    }

    pub fn views(&self) -> Vec<WorkView> {
        // A mode that was saved needs no row; one still working or failed does.
        self.tasks
            .iter()
            .filter(|t| !t.is_dm())
            .filter(|t| !((t.is_mode() || t.is_leave()) && t.step == Step::Done))
            .map(Task::view)
            .collect()
    }

    /// The stored rooms were just read, and say which rooms there are: a finished leave has
    /// done its work (the read does not list the room, or a rejoin since put it back).
    pub fn rooms_read(&mut self) {
        self.tasks
            .retain(|t| !(t.is_leave() && t.step == Step::Done && t.failed.is_none()));
    }

    /// Drop finished tasks whose room is shown.
    pub fn forget_done(&mut self, shown: &dyn Fn(&VerifyingKey) -> bool) {
        self.tasks.retain(|t| {
            !(t.step == Step::Done
                && t.failed.is_none()
                && (t.is_mode() || (!t.is_leave() && t.owner().is_some_and(|o| shown(&o)))))
        });
    }
}

#[cfg(test)]
#[path = "work_tests.rs"]
mod tests;
