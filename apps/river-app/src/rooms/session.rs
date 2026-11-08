//! The background work behind [`super::Link`]: get a grant, connect, read the stored rooms,
//! show them, then subscribe and keep them current. Runs on one worker thread.
//!
//! Policies, in one place:
//! - **A rejected token.** A token the node just issued and has never accepted stops with a
//!   refusal (asking again would only open another approval page). A token that worked
//!   before, or was saved by an earlier run, is discarded and River stops on
//!   [`Access::PermissionRemoved`]: the user removed the permission, so River does not open a
//!   new approval page until they ask it to. If another window saved a newer token in the
//!   meantime, that one is used instead.
//! - **Reading the rejection.** On every connection River reads briefly before it writes, so
//!   a rejection the node sent and then closed on is read before a write could be answered
//!   with a reset (which on Windows discards it). A connection that fails before anything was
//!   heard is retried once at once, reading longer first.
//! - **Reconnecting.** A lost connection is retried with exponential backoff. The backoff is
//!   reset only once a connection has stayed up for a while, so a node that accepts and then
//!   drops every connection is not hammered.
//! - **Subscribing.** A room whose subscribe failed, found nothing, or went unanswered is
//!   named on screen and asked again with its own backoff.
//! - **New and left rooms.** The stored rooms are read again periodically while connected, so
//!   a room joined or left in the browser shows up here without a restart. A read that finds
//!   no store or no rooms at all keeps the list shown: that is a store that went away, not a
//!   user who left every room (leaving keeps a tombstone under the room's key).
//! - **What can't be shown.** A room whose update from the network could not be applied, a
//!   stored room this computer cannot read, and a node with no chat delegate under the key
//!   this build carries are all said on screen ([`Issues`]). A stored room can be unreadable
//!   on Windows alone: a message time past what Windows' clock type holds (about the year
//!   30828) fails to decode there.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{
    ClientError, ClientRequest, ContractError, ContractRequest, ContractResponse, DelegateError,
    ErrorKind, HostResponse, RequestError,
};
use freenet_stdlib::prelude::{ContractInstanceId, UpdateData};
use river_core::chat_delegate::{ChatDelegateResponseMsg, LIST_REFUSED_MARKER};
use river_core::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};
use river_core::stored_rooms::{
    ROOMS_META_KEY, StoredRoom, StoredRoomSlot, StoredRoomsMeta, decode_room_slot,
    decode_rooms_meta, parse_room_storage_key, room_storage_key,
};

use super::delegate::{self, Read};
use super::grant::{GrantOutcome, GrantPacing, GrantToken, NodeHttp, request_grant_with};
use super::room::{LiveRoom, local_minutes};
use super::socket::{NodeSocket, SocketError, TokenVerdict, token_verdict};
use super::token::TokenStore;
pub use crate::chat::LinkStatus;
use crate::chat::Room;

/// How the background work paces itself. Tests shrink these.
#[derive(Clone, Debug)]
pub(super) struct Pace {
    /// The first wait before reconnecting, and the most it doubles to.
    pub backoff_first: Duration,
    pub backoff_max: Duration,
    /// How long a connection must stay up before the backoff starts over.
    pub stable: Duration,
    /// How long to read for an early rejection before the first write, normally and on the
    /// one read-first retry.
    pub early_read: Duration,
    pub early_read_retry: Duration,
    /// How often a subscribed connection wakes to check the limits below.
    pub tick: Duration,
    /// How long the node may be silent before River pings it, and how long the pong may take.
    pub idle_check: Duration,
    pub pong_wait: Duration,
    /// How long the room list says it is catching up before it stops waiting for rooms that
    /// have not answered their subscribe.
    pub catch_up: Duration,
    /// The first wait before asking again for a room whose subscribe failed, and the most it
    /// doubles to.
    pub resubscribe_first: Duration,
    pub resubscribe_max: Duration,
    /// How often the stored rooms are read again while connected.
    pub refresh: Duration,
    /// How long the chat delegate gets to answer a read. The web UI allows 10 seconds per
    /// request (first-time WASM compilation can be slow); a whole read gets a little more.
    pub delegate_deadline: Duration,
    pub grant: GrantPacing,
}

impl Default for Pace {
    fn default() -> Self {
        Pace {
            backoff_first: Duration::from_secs(2),
            backoff_max: Duration::from_secs(30),
            stable: Duration::from_secs(30),
            early_read: Duration::from_millis(300),
            early_read_retry: Duration::from_secs(2),
            tick: Duration::from_secs(5),
            idle_check: Duration::from_secs(60),
            pong_wait: Duration::from_secs(20),
            catch_up: Duration::from_secs(45),
            resubscribe_first: Duration::from_secs(5),
            resubscribe_max: Duration::from_secs(300),
            refresh: Duration::from_secs(60),
            delegate_deadline: Duration::from_secs(20),
            grant: GrantPacing::default(),
        }
    }
}

/// Where getting at the rooms stands, before they can be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// Example data with nothing behind it (snapshots, tests).
    Example,
    /// Opening a connection or reading the rooms.
    Connecting,
    /// The user has to allow River on the page at `approve_url`.
    WaitingForApproval { approve_url: String },
    /// The user said no, or did not answer before the request expired.
    NotApproved { expired: bool },
    /// The permission River had was removed (revoked on Freenet's Apps and permissions page,
    /// or lost by the node). River asks for a new one only when the user says so.
    PermissionRemoved,
    /// The node is too old to grant River its identity.
    Unsupported,
    /// The node refused for a reason the user has to deal with.
    Refused(String),
    /// No node answered. River keeps trying. The text is for logs; the screen says it plainly.
    Unreachable(String),
    /// The rooms are read; the chat screen shows them (or that there are none).
    Ready,
}

impl Access {
    /// Whether the background work has stopped and only the user can restart it.
    pub fn is_stopped(&self) -> bool {
        matches!(
            self,
            Access::NotApproved { .. }
                | Access::PermissionRemoved
                | Access::Unsupported
                | Access::Refused(_)
        )
    }
}

/// What the rooms shown are missing, for the screen to say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Issues {
    /// The node has no chat delegate under the key this build carries. Either River in the
    /// browser never ran on this node, or it runs a newer delegate than this build knows.
    pub store_missing: bool,
    /// Rooms (by name) whose latest from the network could not be applied; they show an
    /// older copy.
    pub not_updated: Vec<String>,
    /// Rooms (by name) River is not subscribed to yet: new messages don't arrive until it is.
    pub not_following: Vec<String>,
    /// Stored rooms this computer could not read at all.
    pub unreadable: usize,
}

/// What the background work reports to the window.
#[derive(Clone, Debug)]
pub enum RoomsUpdate {
    Access(Access),
    /// The rooms, in the room list's order, with the room the web UI had open.
    Rooms {
        rooms: Vec<Room>,
        open: Option<String>,
        status: LinkStatus,
        /// The current time in message time (local minutes since 1970-01-01).
        now: u32,
        issues: Issues,
    },
    /// The connection's state changed while rooms are shown.
    Status(LinkStatus),
}

pub(super) fn run(
    authority: &str,
    config_dir: Option<&Path>,
    report: &dyn Fn(RoomsUpdate),
    pace: &Pace,
    stop: &dyn Fn() -> bool,
) -> Access {
    let store = config_dir.map(TokenStore::in_dir);
    let http = NodeHttp::new(&format!("http://{authority}"));
    let mut token: Option<GrantToken> = None;
    // The token was issued by the node in this run and the node has not accepted it yet.
    let mut unconfirmed = false;
    let mut rooms = Rooms::default();
    let mut backoff = pace.backoff_first;
    let mut early_read = pace.early_read;
    report(RoomsUpdate::Access(Access::Connecting));
    loop {
        if stop() {
            return Access::Connecting;
        }
        let current = match token.clone().or_else(|| store.as_ref()?.load()) {
            Some(t) => t,
            None => {
                let outcome = request_grant_with(
                    &http,
                    authority,
                    &mut |url| {
                        report(RoomsUpdate::Access(Access::WaitingForApproval {
                            approve_url: url.to_owned(),
                        }))
                    },
                    &pace.grant,
                );
                match outcome {
                    GrantOutcome::Approved(t) => {
                        if let Some(store) = &store
                            && let Err(e) = store.save(&t)
                        {
                            // Keep using it for this run; the next run asks again.
                            eprintln!("River: {e}");
                        }
                        unconfirmed = true;
                        report(RoomsUpdate::Access(Access::Connecting));
                        t
                    }
                    GrantOutcome::Denied => return Access::NotApproved { expired: false },
                    GrantOutcome::Expired => return Access::NotApproved { expired: true },
                    GrantOutcome::Unsupported => return Access::Unsupported,
                    GrantOutcome::Refused(m) => return Access::Refused(m),
                    GrantOutcome::Unreachable(e) => {
                        rooms.lost(&e, report);
                        pause(&mut backoff, pace, stop);
                        continue;
                    }
                }
            }
        };
        token = Some(current.clone());
        let ended = follow(
            authority,
            &current,
            &mut rooms,
            report,
            &mut backoff,
            &mut unconfirmed,
            early_read,
            pace,
            stop,
        );
        if stop() {
            return Access::Connecting;
        }
        let err = ended.error;
        match token_verdict(&err) {
            TokenVerdict::Discard => {
                if let Some(newer) = store.as_ref().and_then(|s| s.discard(&current)) {
                    // Another window saved a newer grant; try that one.
                    token = Some(newer);
                    unconfirmed = false;
                    continue;
                }
                if unconfirmed {
                    return Access::Refused(
                        "Freenet didn't accept the permission it just gave River. Try again."
                            .into(),
                    );
                }
                return Access::PermissionRemoved;
            }
            TokenVerdict::Unusable => {
                return Access::Refused(format!(
                    "Freenet won't let River use its permission over this connection. River \
                     has to reach Freenet directly on this computer, at {authority}."
                ));
            }
            TokenVerdict::Refused => return Access::Refused(format!("{err}.")),
            TokenVerdict::Retry => {
                if let SocketError::Failed(m) = &err
                    && m.starts_with(REFUSED_PREFIX)
                {
                    return Access::Refused(m.clone());
                }
                // Nothing was heard on a connection that failed: the node may have rejected
                // the token and dropped the connection faster than the early read waited. Try
                // once more at once, reading longer before the first write.
                if !ended.heard_anything && early_read == pace.early_read {
                    early_read = pace.early_read_retry;
                    continue;
                }
                early_read = pace.early_read;
                rooms.lost(&err.to_string(), report);
                pause(&mut backoff, pace, stop);
            }
        }
    }
}

const REFUSED_PREFIX: &str = "River's storage on this computer refused";

/// Wait out the backoff (waking early to stop), then double it.
fn pause(backoff: &mut Duration, pace: &Pace, stop: &dyn Fn() -> bool) {
    let until = Instant::now() + *backoff;
    while Instant::now() < until && !stop() {
        thread::sleep((until - Instant::now()).min(Duration::from_millis(20)));
    }
    *backoff = (*backoff * 2).min(pace.backoff_max);
}

/// How a connection ended.
struct Ended {
    error: SocketError,
    /// Whether anything at all arrived on it.
    heard_anything: bool,
}

/// Connect, read the rooms, then follow them until the connection fails.
/// `unconfirmed` is cleared once the node has accepted the token.
#[allow(clippy::too_many_arguments)]
fn follow(
    authority: &str,
    token: &GrantToken,
    rooms: &mut Rooms,
    report: &dyn Fn(RoomsUpdate),
    backoff: &mut Duration,
    unconfirmed: &mut bool,
    early_read: Duration,
    pace: &Pace,
    stop: &dyn Fn() -> bool,
) -> Ended {
    let mut socket = match NodeSocket::connect(authority, token) {
        Ok(s) => s,
        Err(error) => {
            return Ended {
                error,
                heard_anything: false,
            };
        }
    };
    let error = follow_on(
        &mut socket,
        rooms,
        report,
        backoff,
        unconfirmed,
        early_read,
        pace,
        stop,
    );
    Ended {
        error,
        heard_anything: socket.heard_anything(),
    }
}

/// Room contracts River has asked to subscribe to, and when to ask again for those whose
/// subscribe failed.
#[derive(Default)]
struct Subscriptions {
    /// Asked, no answer yet.
    pending: HashSet<ContractInstanceId>,
    /// Failed or unanswered: how many times, and when to ask again.
    retry: HashMap<ContractInstanceId, (u32, Instant)>,
}

impl Subscriptions {
    fn ask(&mut self, socket: &mut NodeSocket, id: ContractInstanceId) -> Result<(), SocketError> {
        let get = ContractRequest::Get {
            key: id,
            return_contract_code: false,
            subscribe: true,
            blocking_subscribe: false,
        };
        socket.send(&ClientRequest::ContractOp(get))?;
        self.pending.insert(id);
        Ok(())
    }

    fn answered(&mut self, id: &ContractInstanceId) {
        self.pending.remove(id);
        self.retry.remove(id);
    }

    /// The subscribe failed, found nothing, or went unanswered: ask again later.
    fn failed(&mut self, id: ContractInstanceId, pace: &Pace) {
        self.pending.remove(&id);
        let attempts = self.retry.get(&id).map_or(0, |(n, _)| *n);
        let wait = pace
            .resubscribe_first
            .saturating_mul(1 << attempts.min(16))
            .min(pace.resubscribe_max);
        self.retry
            .insert(id, (attempts.saturating_add(1), Instant::now() + wait));
    }

    /// Ask again for every room whose wait is over.
    fn ask_due(&mut self, socket: &mut NodeSocket) -> Result<(), SocketError> {
        let now = Instant::now();
        let due: Vec<ContractInstanceId> = self
            .retry
            .iter()
            .filter(|(id, (_, at))| *at <= now && !self.pending.contains(*id))
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.ask(socket, id)?;
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn follow_on(
    socket: &mut NodeSocket,
    rooms: &mut Rooms,
    report: &dyn Fn(RoomsUpdate),
    backoff: &mut Duration,
    unconfirmed: &mut bool,
    early_read: Duration,
    pace: &Pace,
    stop: &dyn Fn() -> bool,
) -> SocketError {
    let connected_at = Instant::now();
    if let Err(e) = socket.read_early(early_read) {
        return e;
    }
    let loaded = match load(socket, pace) {
        Ok(l) => l,
        Err(e) => return e,
    };
    *unconfirmed = false;
    rooms.replace(loaded);
    // Every room is subscribed afresh on every connection: subscriptions belong to it.
    let mut subs = Subscriptions::default();
    for id in rooms.contract_ids() {
        if let Err(e) = subs.ask(socket, id) {
            return e;
        }
    }
    rooms.report(LinkStatus::Loading, report);
    report(RoomsUpdate::Access(Access::Ready));
    if let Err(e) = socket.set_read_timeout(pace.tick) {
        return e;
    }
    let mut status = LinkStatus::Loading;
    let mut stable = false;
    let mut ping_at: Option<Instant> = None;
    let mut last_read = Instant::now();
    let mut reload: Option<(Loader, Instant)> = None;
    loop {
        if stop() {
            return SocketError::Failed("stopped".into());
        }
        if !stable && connected_at.elapsed() >= pace.stable {
            stable = true;
            *backoff = pace.backoff_first;
        }
        // A ping that went unanswered means the node, or the path to it, is gone.
        match ping_at {
            Some(at) if socket.last_heard() >= at => ping_at = None,
            Some(at) if at.elapsed() >= pace.pong_wait => {
                return SocketError::Failed("Freenet stopped answering".into());
            }
            Some(_) => {}
            None if socket.last_heard().elapsed() >= pace.idle_check => {
                if let Err(e) = socket.ping() {
                    return e;
                }
                ping_at = Some(Instant::now());
            }
            None => {}
        }
        if let Err(e) = subs.ask_due(socket) {
            return e;
        }
        // Read the stored rooms again now and then, for rooms joined or left elsewhere.
        if let Some((_, started)) = &reload
            && started.elapsed() >= pace.delegate_deadline
        {
            eprintln!("River: reading the stored rooms again took too long; will retry");
            reload = None;
            last_read = Instant::now();
        }
        if reload.is_none() && last_read.elapsed() >= pace.refresh {
            let (loader, reads) = Loader::start();
            for read in reads {
                if let Err(e) = socket.send(&delegate::request(&read)) {
                    return e;
                }
            }
            reload = Some((loader, Instant::now()));
        }

        let response = match socket.recv() {
            Ok(r) => Some(r),
            Err(SocketError::Timeout) => None,
            Err(e) => return e,
        };
        let mut changed = false;
        match response {
            None => {}
            Some(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                key,
                state,
                ..
            }))) => {
                subs.answered(key.id());
                changed = rooms.following(key.id(), true);
                changed |= rooms.merge_bytes(key.id(), state.as_ref());
            }
            Some(Ok(HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                key,
                update,
            }))) => changed = rooms.apply_update(key.id(), &update),
            // The network has no copy. The web UI would seed one with a PUT; this app only
            // reads, so it keeps showing the stored copy and asks again later.
            Some(Ok(HostResponse::ContractResponse(ContractResponse::NotFound {
                instance_id,
            }))) => {
                subs.failed(instance_id, pace);
                changed = rooms.following(&instance_id, false);
            }
            Some(Ok(other)) => {
                if let Some((loader, _)) = &mut reload {
                    for answer in delegate::answers(&other) {
                        match loader.on_answer(answer) {
                            Step::Send(reads) => {
                                for read in reads {
                                    if let Err(e) = socket.send(&delegate::request(&read)) {
                                        return e;
                                    }
                                }
                            }
                            Step::Wait => {}
                            Step::Done(loaded) => {
                                for id in rooms.replace(*loaded) {
                                    if let Err(e) = subs.ask(socket, id) {
                                        return e;
                                    }
                                }
                                reload = None;
                                last_read = Instant::now();
                                changed = true;
                                break;
                            }
                        }
                    }
                }
            }
            Some(Err(e)) => {
                if let Some(id) = failed_contract(&e) {
                    // A subscribe GET that failed: the room stays on its copy, asked again.
                    subs.failed(id, pace);
                    changed = rooms.following(&id, false);
                } else if reload.is_some() {
                    match delegate_error(&e) {
                        // The store went away: keep the list shown (see the module docs).
                        Ok(()) => eprintln!("River: the stored rooms are gone; keeping the list"),
                        Err(e) => eprintln!("River: reading the stored rooms again failed: {e}"),
                    }
                    reload = None;
                    last_read = Instant::now();
                }
            }
        }
        // "Catching up" ends when every room has answered, or after a while regardless. Rooms
        // that never answered are asked again later and named on screen meanwhile.
        if status == LinkStatus::Loading
            && (subs.pending.is_empty() || connected_at.elapsed() >= pace.catch_up)
        {
            for id in std::mem::take(&mut subs.pending) {
                subs.failed(id, pace);
                rooms.following(&id, false);
            }
            status = LinkStatus::Live;
            changed = true;
        }
        if changed {
            rooms.report(status, report);
        }
    }
}

/// The room contract a failed request names, if any.
fn failed_contract(e: &ClientError) -> Option<ContractInstanceId> {
    match e.kind() {
        ErrorKind::RequestError(RequestError::ContractError(c)) => match c {
            ContractError::Get { key, .. } | ContractError::Subscribe { key, .. } => {
                Some(*key.id())
            }
            ContractError::MissingContract { key } => Some(*key),
            _ => None,
        },
        _ => None,
    }
}

/// What one read of the chat delegate found.
struct Loaded {
    slots: Vec<StoredRoom>,
    meta: StoredRoomsMeta,
    /// The delegate is not on this node.
    store_missing: bool,
    /// How many `room:` keys the store listed (tombstones included).
    listed: usize,
    /// Stored rooms that could not be decoded.
    unreadable: usize,
}

impl Loaded {
    fn missing() -> Self {
        Loaded {
            slots: Vec::new(),
            meta: StoredRoomsMeta::default(),
            store_missing: true,
            listed: 0,
            unreadable: 0,
        }
    }
}

/// Reading the stored rooms from the chat delegate, one answer at a time: list the keys, then
/// get each room and the list's view preferences. Answers are matched by storage key, as the
/// web UI matches them, so their order does not matter.
struct Loader {
    /// `None` until the key list has arrived.
    wanted: Option<HashMap<Vec<u8>, Option<Vec<u8>>>>,
    rooms: Vec<VerifyingKey>,
}

enum Step {
    Send(Vec<Read>),
    Wait,
    Done(Box<Loaded>),
}

impl Loader {
    fn start() -> (Loader, Vec<Read>) {
        (
            Loader {
                wanted: None,
                rooms: Vec::new(),
            },
            vec![Read::List],
        )
    }

    fn on_answer(&mut self, answer: ChatDelegateResponseMsg) -> Step {
        match (&mut self.wanted, answer) {
            (None, ChatDelegateResponseMsg::ListResponse { keys }) => {
                let plan = plan(keys.iter().map(|k| k.as_bytes()));
                let mut wanted: HashMap<Vec<u8>, Option<Vec<u8>>> = plan
                    .rooms
                    .iter()
                    .map(|vk| (room_storage_key(vk), None))
                    .collect();
                if plan.has_meta {
                    wanted.insert(ROOMS_META_KEY.to_vec(), None);
                }
                self.rooms = plan.rooms;
                let reads: Vec<Read> = wanted.keys().cloned().map(Read::Get).collect();
                self.wanted = Some(wanted);
                if reads.is_empty() {
                    return Step::Done(Box::new(self.finish()));
                }
                Step::Send(reads)
            }
            (Some(wanted), ChatDelegateResponseMsg::GetResponse { key, value }) => {
                if let Some(slot) = wanted.get_mut(key.as_bytes())
                    && slot.is_none()
                {
                    *slot = Some(value.unwrap_or_default());
                }
                if wanted.values().all(Option::is_some) {
                    Step::Done(Box::new(self.finish()))
                } else {
                    Step::Wait
                }
            }
            _ => Step::Wait,
        }
    }

    fn finish(&mut self) -> Loaded {
        let mut wanted = self.wanted.take().unwrap_or_default();
        let meta = match wanted.remove(ROOMS_META_KEY) {
            Some(Some(bytes)) if !bytes.is_empty() => {
                decode_rooms_meta(&bytes).unwrap_or_else(|e| {
                    eprintln!("River: {e}");
                    StoredRoomsMeta::default()
                })
            }
            _ => StoredRoomsMeta::default(),
        };
        let mut slots = Vec::new();
        let mut unreadable = 0;
        for vk in &self.rooms {
            let Some(Some(bytes)) = wanted.remove(&room_storage_key(vk)) else {
                continue;
            };
            if bytes.is_empty() {
                continue;
            }
            match decode_room_slot(&bytes) {
                Ok(StoredRoomSlot::Present(room)) => slots.push(*room),
                Ok(StoredRoomSlot::Tombstone) => {}
                // One unreadable room must not hide the others; the screen counts it.
                Err(e) => {
                    eprintln!("River: skipping a stored room: {e}");
                    unreadable += 1;
                }
            }
        }
        Loaded {
            slots,
            meta,
            store_missing: false,
            listed: self.rooms.len(),
            unreadable,
        }
    }
}

/// Read the stored rooms, blocking until done or the deadline.
fn load(socket: &mut NodeSocket, pace: &Pace) -> Result<Loaded, SocketError> {
    socket.set_read_timeout(pace.delegate_deadline)?;
    let deadline = Instant::now() + pace.delegate_deadline;
    let (mut loader, reads) = Loader::start();
    for read in reads {
        socket.send(&delegate::request(&read))?;
    }
    loop {
        if Instant::now() >= deadline {
            return Err(SocketError::Failed(
                "River's storage on this computer didn't answer in time".into(),
            ));
        }
        let response = match socket.recv() {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                delegate_error(&e)?;
                // The delegate is not on this node.
                return Ok(Loaded::missing());
            }
            Err(SocketError::Timeout) => continue,
            Err(e) => return Err(e),
        };
        // One response can carry several answers; take every one.
        for answer in delegate::answers(&response) {
            match loader.on_answer(answer) {
                Step::Send(reads) => {
                    for read in reads {
                        socket.send(&delegate::request(&read))?;
                    }
                }
                Step::Wait => {}
                Step::Done(loaded) => return Ok(*loaded),
            }
        }
    }
}

/// `Ok(())` when the error only says the delegate is missing; the error to stop on otherwise.
fn delegate_error(e: &ClientError) -> Result<(), SocketError> {
    if let ErrorKind::RequestError(RequestError::DelegateError(DelegateError::Missing(_))) =
        e.kind()
    {
        return Ok(());
    }
    let text = e.to_string();
    // The untyped wordings older nodes use for a delegate that is not installed (the web
    // UI's `is_missing_delegate_error`): "delegate <key> not found in store" and
    // "missing delegate <key>", here without regard to case.
    let lower = text.to_lowercase();
    if lower.contains("missing delegate")
        || (lower.contains("delegate") && lower.contains("not found in store"))
    {
        return Ok(());
    }
    if text.contains(LIST_REFUSED_MARKER) {
        return Err(SocketError::Failed(format!(
            "{REFUSED_PREFIX} to list your rooms for this app."
        )));
    }
    Err(SocketError::Failed(format!(
        "Freenet couldn't read your rooms: {text}"
    )))
}

/// What a `ListResponse` says to read: the per-room keys, and whether `rooms_meta` exists.
#[derive(Debug, PartialEq)]
struct Plan {
    rooms: Vec<VerifyingKey>,
    has_meta: bool,
}

fn plan<'a>(keys: impl Iterator<Item = &'a [u8]>) -> Plan {
    let mut plan = Plan {
        rooms: Vec::new(),
        has_meta: false,
    };
    for key in keys {
        if let Some(vk) = parse_room_storage_key(key) {
            if !plan.rooms.contains(&vk) {
                plan.rooms.push(vk);
            }
        } else if key == ROOMS_META_KEY {
            plan.has_meta = true;
        }
    }
    plan
}

/// The rooms being shown, with what is needed to keep them current.
#[derive(Default)]
struct Rooms {
    /// In the room list's order.
    list: Vec<LiveRoom>,
    open: Option<String>,
    /// Whether the rooms have been read at least once this run.
    loaded: bool,
    store_missing: bool,
    unreadable: usize,
    /// Rooms whose latest from the network could not be applied.
    not_updated: HashSet<VerifyingKey>,
    /// Rooms not subscribed to yet.
    not_following: HashSet<VerifyingKey>,
}

impl Rooms {
    /// Take freshly read rooms, returning the room contracts that are new to the list.
    ///
    /// After the first read, a read that found no store, or no `room:` keys at all, keeps the
    /// list as it is: that is a store gone missing, not a user who left every room.
    ///
    /// A room already shown keeps its copy, which has the network merged in, and the stored
    /// copy is merged into it (the web UI may have written something newer); it takes the
    /// slot's read position. If the stored slot now names a different contract or a
    /// different identity or secrets (the room was re-keyed, or rejoined), the stored slot is
    /// taken instead, with the old copy merged in when the contract is the same.
    fn replace(&mut self, loaded: Loaded) -> Vec<ContractInstanceId> {
        if self.loaded && (loaded.store_missing || loaded.listed == 0) && !self.list.is_empty() {
            eprintln!("River: the stored rooms read as empty; keeping the list shown");
            return Vec::new();
        }
        let mut old: HashMap<VerifyingKey, LiveRoom> = self
            .list
            .drain(..)
            .map(|r| (r.stored.owner_vk, r))
            .collect();
        let mut new_ids = Vec::new();
        let mut by_key: HashMap<VerifyingKey, LiveRoom> = HashMap::new();
        for slot in loaded.slots {
            let vk = slot.owner_vk;
            let room = match old.remove(&vk) {
                Some(mut kept) if same_room(&kept.stored, &slot) => {
                    kept.stored.last_read_message_id = slot.last_read_message_id.clone();
                    if let Err(e) = kept.merge_state(&slot.room_state) {
                        eprintln!("River: the stored copy of a room was not merged: {e}");
                    }
                    kept
                }
                Some(previous) => {
                    let mut fresh = LiveRoom::new(slot);
                    if fresh.contract_id() == previous.contract_id() {
                        if let Err(e) = fresh.merge_state(&previous.stored.room_state) {
                            eprintln!("River: a room's earlier copy was not merged: {e}");
                            self.not_updated.insert(vk);
                        }
                    } else {
                        new_ids.push(fresh.contract_id());
                    }
                    fresh
                }
                None => {
                    let fresh = LiveRoom::new(slot);
                    new_ids.push(fresh.contract_id());
                    fresh
                }
            };
            by_key.insert(vk, room);
        }
        for vk in order(&loaded.meta.room_order, by_key.keys().copied().collect()) {
            if let Some(room) = by_key.remove(&vk) {
                self.list.push(room);
            }
        }
        let shown: HashSet<VerifyingKey> = self.list.iter().map(|r| r.stored.owner_vk).collect();
        self.not_updated.retain(|vk| shown.contains(vk));
        self.not_following.retain(|vk| shown.contains(vk));
        self.open = loaded
            .meta
            .current_room_key
            .filter(|k| shown.contains(k))
            .map(|k| bs58::encode(k.to_bytes()).into_string());
        self.store_missing = loaded.store_missing;
        self.unreadable = loaded.unreadable;
        self.loaded = true;
        new_ids
    }

    fn contract_ids(&self) -> Vec<ContractInstanceId> {
        self.list.iter().map(LiveRoom::contract_id).collect()
    }

    fn by_contract(&mut self, id: &ContractInstanceId) -> Option<&mut LiveRoom> {
        self.list.iter_mut().find(|r| r.contract_id() == *id)
    }

    /// Record whether a room is subscribed; true when that changed what shows.
    fn following(&mut self, id: &ContractInstanceId, yes: bool) -> bool {
        let Some(vk) = self.by_contract(id).map(|r| r.stored.owner_vk) else {
            return false;
        };
        if yes {
            self.not_following.remove(&vk)
        } else {
            self.not_following.insert(vk)
        }
    }

    /// Record whether a room took the network's latest; true when that changed what shows.
    fn outcome(&mut self, id: &ContractInstanceId, result: Result<(), String>) -> bool {
        let Some(vk) = self.by_contract(id).map(|r| r.stored.owner_vk) else {
            return false;
        };
        match result {
            Ok(()) => {
                self.not_updated.remove(&vk);
                true
            }
            Err(e) => {
                eprintln!("River: a room's update from the network was not applied: {e}");
                self.not_updated.insert(vk)
            }
        }
    }

    fn merge_bytes(&mut self, id: &ContractInstanceId, bytes: &[u8]) -> bool {
        let Some(room) = self.by_contract(id) else {
            return false;
        };
        let result = ciborium::de::from_reader::<ChatRoomStateV1, _>(bytes)
            .map_err(|e| format!("unreadable state: {e}"))
            .and_then(|incoming| room.merge_state(&incoming));
        self.outcome(id, result)
    }

    fn apply_update(&mut self, id: &ContractInstanceId, update: &UpdateData<'_>) -> bool {
        match update {
            UpdateData::State(state) => self.merge_bytes(id, state.as_ref()),
            UpdateData::StateAndDelta { state, .. } => self.merge_bytes(id, state.as_ref()),
            UpdateData::Delta(delta) => {
                let Some(room) = self.by_contract(id) else {
                    return false;
                };
                let result = ciborium::de::from_reader::<ChatRoomStateV1Delta, _>(delta.as_ref())
                    .map_err(|e| format!("unreadable delta: {e}"))
                    .and_then(|delta| room.apply_delta(delta));
                self.outcome(id, result)
            }
            // Related-contract updates, and variants newer than this stdlib.
            _ => false,
        }
    }

    fn views(&self) -> Vec<Room> {
        self.list.iter().map(|r| r.view(&local_minutes)).collect()
    }

    fn names(&self, set: &HashSet<VerifyingKey>) -> Vec<String> {
        self.list
            .iter()
            .filter(|r| set.contains(&r.stored.owner_vk))
            .map(LiveRoom::name)
            .collect()
    }

    fn issues(&self) -> Issues {
        Issues {
            store_missing: self.store_missing,
            not_updated: self.names(&self.not_updated),
            not_following: self.names(&self.not_following),
            unreadable: self.unreadable,
        }
    }

    fn report(&self, status: LinkStatus, report: &dyn Fn(RoomsUpdate)) {
        report(RoomsUpdate::Rooms {
            rooms: self.views(),
            open: self.open.clone(),
            status,
            now: local_minutes(SystemTime::now()),
            issues: self.issues(),
        });
    }

    /// The connection is gone: keep showing the rooms if there are any, and say so.
    fn lost(&self, why: &str, report: &dyn Fn(RoomsUpdate)) {
        if self.loaded {
            report(RoomsUpdate::Status(LinkStatus::Reconnecting));
        } else {
            report(RoomsUpdate::Access(Access::Unreachable(why.to_owned())));
        }
    }
}

/// Whether a stored slot is still the room `kept` was read from: the same contract, identity
/// and secrets.
fn same_room(kept: &StoredRoom, slot: &StoredRoom) -> bool {
    kept.contract_key == slot.contract_key
        && kept.self_sk.as_ref().map(|k| k.to_bytes())
            == slot.self_sk.as_ref().map(|k| k.to_bytes())
        && kept.self_vk == slot.self_vk
        && kept.invitation_secrets == slot.invitation_secrets
        && kept.self_nickname == slot.self_nickname
}

/// The web UI's room order (`Rooms::ordered_room_keys`): the stored order first, then the
/// rest by key bytes.
fn order(room_order: &[VerifyingKey], mut present: Vec<VerifyingKey>) -> Vec<VerifyingKey> {
    let mut out: Vec<VerifyingKey> = Vec::new();
    for k in room_order {
        if present.contains(k) && !out.contains(k) {
            out.push(*k);
        }
    }
    present.retain(|k| !out.contains(k));
    present.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    out.extend(present);
    out
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
