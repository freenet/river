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
//!   named on screen and asked again with its own backoff. A late answer to an earlier ask
//!   can leave the node notifying twice; applying the same update twice changes nothing,
//!   since a room's state merges idempotently. Left and re-keyed rooms are no longer asked for.
//!   This client API has no unsubscribe, so each ask's listener on the node stays registered
//!   until the connection closes; a reconnect starts with none.
//! - **New and left rooms.** The stored rooms are read again periodically while connected, so
//!   a room joined or left in the browser shows up here without a restart. A read that finds
//!   no store or no rooms at all keeps the list shown: that is a store that went away, not a
//!   user who left every room (leaving keeps a tombstone under the room's key).
//! - **Sending.** What the user does in a room arrives as a [`Command`] and is shown at once,
//!   marked as on its way, then sent as an UPDATE, one per room at a time ([`super::outbox`]
//!   has the details and what it mirrors in the web UI). The node's answer decides: taken,
//!   refused (shown as not sent, with the node's reason), or unanswered for
//!   [`Pace::update_wait`] (shown as not confirmed). An answer names only the room, so a room
//!   that owes one sends nothing more on this connection; a retry of an unanswered update, or
//!   an answer owed for [`Pace::owed_limit`], reconnects, since a new connection owes
//!   nothing. What was out when the connection dropped goes again on the next one, unchanged.
//!   Commands that arrive while disconnected are shown and wait; commands that arrive before
//!   the rooms were first read wait for them. What is unsent outlives the run (a stop, or a
//!   panic), and a first read that finds no rooms does not drop it.
//! - **Setting up River's storage.** A connection first lists the stored rooms, as the web UI
//!   does since freenet/river#757. If the node says the chat delegate is missing (River never
//!   ran on this node, or the node lost it), the app registers the delegate this build carries,
//!   waits for the node's answer for up to [`Pace::register_wait`], and lists again. Only a
//!   grant connection can register ([`super::socket`] has why that matters). A refused or
//!   unanswered registration does not stop the read, as in the web UI; a store still missing
//!   then is said on screen, and the re-reads do not register it again (each would re-send the
//!   whole WASM). A new connection, or a create or join the user asks for, tries again. A store
//!   that was there and goes missing later in a connection ends it, and the next connection
//!   registers before it lists.
//! - **A newer River.** Each run checks River's pointer records ([`super::generation`]) on a
//!   connection of its own, before the first write and every [`Pace::pointers`]`.recheck`
//!   after. Writes (registering, sending, creating, joining, the signing key sweep) wait until
//!   every pointer has answered, or for `first_wait`, then go on without an answer. The wait
//!   never blocks the connection: reads, subscriptions and the user's commands go on. A store
//!   that needs registering meanwhile is registered once the wait is over, on a new
//!   connection, which registers before anything else (as when the store goes missing); the
//!   screen says River is setting up its storage. Once a verified record names a newer
//!   delegate or room contract than this build carries, nothing is written again: what the
//!   user sends, creates or joins fails at once with why, the composer is blocked with why,
//!   and the store is not registered. The rooms still show and still receive. Each
//!   connection's write gate refuses writes too.
//! - **What can't be shown.** A room whose update from the network could not be applied, a
//!   stored room this computer cannot read, and a node with no chat delegate under the key
//!   this build carries are all said on screen ([`Issues`]). A stored room can be unreadable
//!   on Windows alone: a message time past what Windows' clock type holds (about the year
//!   30828) fails to decode there.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{
    ClientError, ClientRequest, ContractError, ContractRequest, ContractResponse, DelegateError,
    ErrorKind, HostResponse, RequestError,
};
use freenet_stdlib::prelude::{ContractInstanceId, StateDelta, UpdateData};
use river_core::chat_delegate::{ChatDelegateResponseMsg, LIST_REFUSED_MARKER};
use river_core::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};
use river_core::stored_rooms::{
    ROOMS_META_KEY, StoredRoom, StoredRoomSlot, StoredRoomsMeta, decode_room_slot,
    decode_rooms_meta, parse_room_storage_key, room_storage_key,
};

use super::delegate::{self, Read};
use super::generation::{
    CheckPace, UPDATE_REQUIRED, UPDATE_REQUIRED_SEND, UPDATE_REQUIRED_WORK, Watch,
};
use super::grant::{GrantOutcome, GrantPacing, GrantToken, NodeHttp, request_grant_with};
use super::modes::{NotificationMode, decode_modes};
use super::outbox::{Action, Command, Outbox};
use super::room::{LiveRoom, local_minutes};
use super::socket::{NodeSocket, SocketError, TokenVerdict, token_verdict};
use super::token::TokenStore;
use super::work::WorkView;
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
    /// How often a subscribed connection wakes to check the limits below and to take what
    /// the user did, so this is also how long a sent message can wait to go out.
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
    /// How long the node may take to answer an update before the message counts as not sent.
    pub update_wait: Duration,
    /// How long an answer may be owed before the connection is dropped for it (three times
    /// `update_wait` by default).
    pub owed_limit: Duration,
    /// How long the chat delegate gets to answer a read. The web UI allows 10 seconds per
    /// request (first-time WASM compilation can be slow); a whole read gets a little more.
    pub delegate_deadline: Duration,
    /// How long the node may take to answer registering the chat delegate before the rooms are
    /// read anyway (the web UI's `REGISTER_ACK_TIMEOUT_MS`).
    pub register_wait: Duration,
    /// How long after a `Missing` answer the stored rooms are read again. That answer arms the
    /// node's per-connection backoff for the delegate's other requests (100 ms after one
    /// failure, +-20%, freenet-core `DelegateRateLimiter`), and a read inside it is refused as
    /// rate limited. The web UI waits the same 250 ms (`MISSING_BACKOFF_LAPSE_MS`).
    pub missing_lapse: Duration,
    /// While the store could not be set up, how long until the app registers it again by
    /// itself, and the most that doubles to. A node that was slow or refused at startup and
    /// is healthy later is set up without the user doing anything, at most this often.
    pub register_retry_first: Duration,
    pub register_retry_max: Duration,
    /// After a failure about a room's signing key, how long until the app asks about that room
    /// again, and the most that doubles to ([`super::keys`]).
    pub key_retry_first: Duration,
    pub key_retry_max: Duration,
    /// How long after the stored rooms were read the signing key sweep waits before it starts.
    pub key_delay: Duration,
    /// How long an update's unanswered answer holds the signing key sweep back. After that the
    /// update is taken to be lost, and the sweep goes on.
    pub key_owed_cap: Duration,
    pub grant: GrantPacing,
    /// Checking River's pointer records for a newer River ([`super::generation`]).
    pub pointers: CheckPace,
}

impl Default for Pace {
    fn default() -> Self {
        Pace {
            backoff_first: Duration::from_secs(2),
            backoff_max: Duration::from_secs(30),
            stable: Duration::from_secs(30),
            early_read: Duration::from_millis(300),
            early_read_retry: Duration::from_secs(2),
            tick: Duration::from_millis(100),
            idle_check: Duration::from_secs(60),
            pong_wait: Duration::from_secs(20),
            catch_up: Duration::from_secs(45),
            resubscribe_first: Duration::from_secs(5),
            resubscribe_max: Duration::from_secs(300),
            refresh: Duration::from_secs(60),
            update_wait: Duration::from_secs(60),
            owed_limit: Duration::from_secs(180),
            delegate_deadline: Duration::from_secs(20),
            register_wait: Duration::from_secs(15),
            missing_lapse: Duration::from_millis(250),
            register_retry_first: Duration::from_secs(60),
            register_retry_max: Duration::from_secs(15 * 60),
            key_retry_first: Duration::from_secs(60),
            key_retry_max: Duration::from_secs(30 * 60),
            key_delay: Duration::from_secs(5),
            key_owed_cap: Duration::from_secs(30),
            grant: GrantPacing::default(),
            pointers: CheckPace::default(),
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
    /// The node has no chat delegate under the key this build carries, even after River
    /// registered it.
    pub store_missing: bool,
    /// Rooms (by name) whose latest from the network could not be applied; they show an
    /// older copy.
    pub not_updated: Vec<String>,
    /// Rooms (by name) River is not subscribed to yet: new messages don't arrive until it is.
    pub not_following: Vec<String>,
    /// Stored rooms this computer could not read at all.
    pub unreadable: usize,
    /// River has moved to a newer chat delegate or room contract than this app carries: it
    /// writes nothing, and the user needs to update it.
    pub update_required: bool,
    /// River's storage is not set up yet: the app is checking River's version first.
    pub setting_up: bool,
}

/// What the background work reports to the window. Its `Debug` hides an invitation's link,
/// which is a credential.
#[derive(Clone)]
pub enum RoomsUpdate {
    Access(Access),
    /// The rooms, in the room list's order, with the room the web UI had open.
    Rooms {
        rooms: Vec<Room>,
        open: Option<String>,
        /// Each room's notification mode by room key; a room not named is `All`.
        modes: HashMap<String, NotificationMode>,
        /// Rooms whose mode pick ended (stored, or failed) since the last update: the screen
        /// stops holding its own pick and follows `modes`.
        settled: Vec<String>,
        status: LinkStatus,
        /// The current time in message time (local minutes since 1970-01-01).
        now: u32,
        issues: Issues,
        /// Rooms being created or joined, and those that failed.
        work: Vec<WorkView>,
    },
    /// The connection's state changed while rooms are shown.
    Status(LinkStatus),
    /// The invitation the user asked for in the room keyed `room`: its link, or why there is
    /// none.
    Invite {
        room: String,
        link: Result<String, String>,
    },
}

impl std::fmt::Debug for RoomsUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoomsUpdate::Access(a) => f.debug_tuple("Access").field(a).finish(),
            RoomsUpdate::Rooms {
                rooms,
                open,
                modes,
                settled,
                status,
                now,
                issues,
                work,
            } => f
                .debug_struct("Rooms")
                .field("rooms", rooms)
                .field("open", open)
                .field("modes", modes)
                .field("settled", settled)
                .field("status", status)
                .field("now", now)
                .field("issues", issues)
                .field("work", work)
                .finish(),
            RoomsUpdate::Status(s) => f.debug_tuple("Status").field(s).finish(),
            RoomsUpdate::Invite { room, link } => f
                .debug_struct("Invite")
                .field("room", room)
                .field("link", &link.as_ref().map(|_| "<hidden>"))
                .finish(),
        }
    }
}

/// Run until stopped, or until a state that needs the user. `outbox` is what the user sent
/// that the node has not taken; it is kept across runs, so a stop does not lose it, and what
/// was in flight goes again on the next run.
#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    authority: &str,
    config_dir: Option<&Path>,
    report: &dyn Fn(RoomsUpdate),
    pace: &Pace,
    stop: &dyn Fn() -> bool,
    commands: &Receiver<Command>,
    outbox: &mut Outbox,
    watch: &Watch,
) -> Access {
    let mut kept = KeepOutbox {
        rooms: Rooms {
            outbox: std::mem::take(outbox),
            web_app: format!(
                "http://{authority}/v1/contract/web/{}/",
                super::grant::RIVER_WEB_APP
            ),
            ..Rooms::default()
        },
        outbox,
    };
    run_with(
        authority,
        config_dir,
        report,
        pace,
        stop,
        commands,
        &mut kept.rooms,
        watch,
    )
}

/// Gives the outbox back to the link when a run ends, however it ends: a panic too (the link
/// catches it), so what the user sent is not lost with the run.
struct KeepOutbox<'a> {
    rooms: Rooms,
    outbox: &'a mut Outbox,
}

impl Drop for KeepOutbox<'_> {
    fn drop(&mut self) {
        self.rooms.outbox.requeue();
        *self.outbox = std::mem::take(&mut self.rooms.outbox);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_with(
    authority: &str,
    config_dir: Option<&Path>,
    report: &dyn Fn(RoomsUpdate),
    pace: &Pace,
    stop: &dyn Fn() -> bool,
    commands: &Receiver<Command>,
    rooms: &mut Rooms,
    watch: &Watch,
) -> Access {
    let store = config_dir.map(TokenStore::in_dir);
    let http = NodeHttp::new(&format!("http://{authority}"));
    let mut token: Option<GrantToken> = None;
    // The token was issued by the node in this run and the node has not accepted it yet.
    let mut unconfirmed = false;
    let mut backoff = pace.backoff_first;
    let mut early_read = pace.early_read;
    // Whether the read-first retry below was just taken.
    let mut read_first_retry = false;
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
                        pause(&mut backoff, pace, stop, &mut || {
                            rooms.idle(commands, report)
                        });
                        continue;
                    }
                }
            }
        };
        token = Some(current.clone());
        let ended = follow(
            &Watching {
                watch,
                authority,
                token: &current,
            },
            rooms,
            report,
            &mut backoff,
            &mut unconfirmed,
            early_read,
            pace,
            stop,
            commands,
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
                    read_first_retry = false;
                    early_read = pace.early_read;
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
                if ended.connected && !ended.heard_anything && !read_first_retry {
                    read_first_retry = true;
                    early_read = pace.early_read_retry;
                    continue;
                }
                read_first_retry = false;
                early_read = pace.early_read;
                rooms.lost(&err.to_string(), report);
                pause(&mut backoff, pace, stop, &mut || {
                    rooms.idle(commands, report)
                });
            }
        }
    }
}

const REFUSED_PREFIX: &str = "River's storage on this computer refused";

/// Wait out the backoff (waking early to stop, and to call `idle`), then double it.
fn pause(backoff: &mut Duration, pace: &Pace, stop: &dyn Fn() -> bool, idle: &mut dyn FnMut()) {
    let until = Instant::now() + *backoff;
    while Instant::now() < until && !stop() {
        idle();
        thread::sleep((until - Instant::now()).min(Duration::from_millis(20)));
    }
    *backoff = (*backoff * 2).min(pace.backoff_max);
}

/// How a connection ended.
struct Ended {
    error: SocketError,
    /// Whether the WebSocket connection opened at all.
    connected: bool,
    /// Whether anything at all arrived on it.
    heard_anything: bool,
}

/// Connect, read the rooms, then follow them until the connection fails.
/// `unconfirmed` is cleared once the node has accepted the token.
#[allow(clippy::too_many_arguments)]
fn follow(
    watching: &Watching,
    rooms: &mut Rooms,
    report: &dyn Fn(RoomsUpdate),
    backoff: &mut Duration,
    unconfirmed: &mut bool,
    early_read: Duration,
    pace: &Pace,
    stop: &dyn Fn() -> bool,
    commands: &Receiver<Command>,
) -> Ended {
    let mut socket =
        match NodeSocket::connect(watching.authority, watching.token, watching.watch.gate()) {
            Ok(s) => s,
            Err(error) => {
                return Ended {
                    error,
                    connected: false,
                    heard_anything: false,
                };
            }
        };
    let error = follow_on(
        &mut socket,
        watching,
        rooms,
        report,
        backoff,
        unconfirmed,
        early_read,
        pace,
        stop,
        commands,
    );
    // Updates belong to the connection: what was out goes again on the next one.
    rooms.outbox.requeue();
    Ended {
        error,
        connected: true,
        heard_anything: socket.heard_anything(),
    }
}

/// The link's [`Watch`], with what a check of the pointers needs: the node and the grant.
struct Watching<'a> {
    watch: &'a Watch,
    authority: &'a str,
    token: &'a GrantToken,
}

impl Watching<'_> {
    /// Take a check's answer, and start one if due. True when River was just found to have
    /// moved on.
    fn poll(&self, pace: &Pace) -> bool {
        self.watch.poll(self.authority, self.token, &pace.pointers)
    }

    /// Whether the first check is settled, so a registration may be decided now.
    fn settled(&self, pace: &Pace) -> bool {
        self.watch.settled(&pace.pointers, Instant::now())
    }

    fn writes_allowed(&self, pace: &Pace) -> bool {
        self.watch.writes_allowed(&pace.pointers)
    }
}

/// Room contracts River has asked to subscribe to, and when to ask again for those whose
/// subscribe failed.
#[derive(Default)]
struct Subscriptions {
    /// Asked, no answer yet, with when they were asked.
    pending: HashMap<ContractInstanceId, Instant>,
    /// The ones asked when the connection was set up and not yet answered or given up on. Only
    /// these hold the signing key sweep back: a room that never answers is asked again and again,
    /// and would hold it back for good.
    connect_time: HashSet<ContractInstanceId>,
    /// Failed or unanswered: how many times, and when to ask again.
    retry: HashMap<ContractInstanceId, (u32, Instant)>,
}

impl Subscriptions {
    /// Ask for a room as the connection is set up.
    fn ask_at_connect(
        &mut self,
        socket: &mut NodeSocket,
        id: ContractInstanceId,
    ) -> Result<(), SocketError> {
        self.ask(socket, id)?;
        self.connect_time.insert(id);
        Ok(())
    }

    fn ask(&mut self, socket: &mut NodeSocket, id: ContractInstanceId) -> Result<(), SocketError> {
        let get = ContractRequest::Get {
            key: id,
            return_contract_code: false,
            subscribe: true,
            blocking_subscribe: false,
        };
        socket.send(&ClientRequest::ContractOp(get))?;
        self.pending.insert(id, Instant::now());
        Ok(())
    }

    /// Asks unanswered for `wait` count as failed, whenever they were made. A room that never
    /// answers is named on screen and asked again later.
    fn expire(&mut self, wait: Duration, pace: &Pace) -> Vec<ContractInstanceId> {
        let late: Vec<ContractInstanceId> = self
            .pending
            .iter()
            .filter(|(_, at)| at.elapsed() >= wait)
            .map(|(id, _)| *id)
            .collect();
        for id in &late {
            self.failed(*id, pace);
        }
        late
    }

    /// Forget rooms no longer shown (left, or re-keyed to another contract), so River stops
    /// asking for them.
    fn keep_only(&mut self, shown: &[ContractInstanceId]) {
        self.pending.retain(|id, _| shown.contains(id));
        self.connect_time.retain(|id| shown.contains(id));
        self.retry.retain(|id, _| shown.contains(id));
    }

    fn answered(&mut self, id: &ContractInstanceId) {
        self.pending.remove(id);
        self.connect_time.remove(id);
        self.retry.remove(id);
    }

    /// The subscribe failed, found nothing, or went unanswered: ask again later.
    fn failed(&mut self, id: ContractInstanceId, pace: &Pace) {
        self.pending.remove(&id);
        self.connect_time.remove(&id);
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
            .filter(|(id, (_, at))| *at <= now && !self.pending.contains_key(*id))
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
    watching: &Watching,
    rooms: &mut Rooms,
    report: &dyn Fn(RoomsUpdate),
    backoff: &mut Duration,
    unconfirmed: &mut bool,
    early_read: Duration,
    pace: &Pace,
    stop: &dyn Fn() -> bool,
    commands: &Receiver<Command>,
) -> SocketError {
    let connected_at = Instant::now();
    // Whatever the signing key sweep had out went with the last connection: a failure of it.
    let key_pace = (pace.key_retry_first, pace.key_retry_max);
    rooms.keys.connection_lost(key_pace, Instant::now());
    if let Err(e) = socket.read_early(early_read) {
        return e;
    }
    // The pointer check for this connection goes out now, beside the first reads.
    watching.poll(pace);
    // A registration put off on an earlier connection is decided afresh on this one.
    rooms.register_deferred = false;
    // The store went missing on the last connection: register before anything else.
    let mut registered = false;
    if std::mem::take(&mut rooms.register_first) {
        if !watching.settled(pace) {
            rooms.register_deferred = true;
        } else if !watching.watch.is_stale() {
            if let Err(e) = register_logged(socket, pace) {
                return e;
            }
            registered = true;
        }
    }
    let mut loaded = match load(socket, pace) {
        Ok(l) => l,
        Err(e) => return e,
    };
    if loaded.store_missing && !registered && !watching.settled(pace) {
        // Not decided yet whether this build may set up River's storage: read on, and register
        // on a new connection once it is (below, in the loop).
        rooms.register_deferred = true;
    } else if loaded.store_missing && !registered && !watching.watch.is_stale() {
        // The node has no chat delegate: set it up, then read again once the backoff its
        // `Missing` armed has lapsed.
        let missing_at = Instant::now();
        if let Err(e) = register_logged(socket, pace) {
            return e;
        }
        let lapse = pace.missing_lapse.saturating_sub(missing_at.elapsed());
        if !lapse.is_zero() {
            thread::sleep(lapse);
        }
        loaded = match load(socket, pace) {
            Ok(l) => l,
            Err(e) => return e,
        };
    }
    *unconfirmed = false;
    rooms.replace(loaded);
    rooms.update_required = watching.watch.is_stale();
    // Every room is subscribed afresh on every connection: subscriptions belong to it.
    let mut subs = Subscriptions::default();
    for id in rooms.contract_ids() {
        if let Err(e) = subs.ask_at_connect(socket, id) {
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
        // What the user did, then the updates it makes.
        let mut changed = rooms.take_commands(commands);
        // A newer River: nothing more is written, and what waits to be fails, saying why.
        // Until the first check answers (or its wait is over) writes only wait.
        changed |= watching.poll(pace);
        let writes = watching.writes_allowed(pace);
        if watching.watch.is_stale() {
            changed |= !rooms.update_required;
            rooms.update_required = true;
            changed |= rooms.outbox.fail_queued(UPDATE_REQUIRED);
            changed |= rooms.outbox.work.halt(UPDATE_REQUIRED_WORK);
        }
        // A registration put off for the first check: once it is settled, register on a new
        // connection, which does that before it sends anything else. A connection that only
        // read so far has nothing out that a reconnect could lose. A build River has moved past
        // does not register at all.
        if rooms.register_deferred && watching.settled(pace) {
            rooms.register_deferred = false;
            changed = true;
            if !watching.watch.is_stale() {
                rooms.register_first = true;
                rooms.report(status, report);
                return SocketError::Failed(
                    "registering River's storage now that its version is checked".into(),
                );
            }
        }
        changed |= rooms.outbox.expire(pace.update_wait, Instant::now());
        // A room that owes an answer sends nothing more on this connection; a new one owes
        // nothing. So a retry of an unanswered update, or an answer owed for too long, is a
        // reconnect.
        if rooms
            .outbox
            .wants_reconnect(pace.owed_limit, Instant::now())
        {
            if changed {
                rooms.report(status, report);
            }
            return SocketError::Failed("reconnecting to send again".into());
        }
        // A create or join whose request went unanswered: its answer must not arrive late and
        // be taken for the next request's, so the task goes on on a new connection.
        if rooms
            .outbox
            .work
            .expire(pace.delegate_deadline, pace.update_wait, Instant::now())
        {
            rooms.report(status, report);
            return SocketError::Failed("reconnecting to finish creating or joining a room".into());
        }
        if writes {
            match rooms.send_updates(socket) {
                Ok(failed) => changed |= failed,
                Err(e) => return e,
            }
        }
        // The create or join under way sends its next request, unless the stored rooms are
        // being read: their answers and its are told apart by kind, but an error names neither.
        if writes
            && let Some(request) = rooms
                .outbox
                .work
                .next_request(reload.is_some() || rooms.keys.busy(), Instant::now())
            && let Err(e) = socket.send(&request)
        {
            return e;
        }
        // Read the stored rooms again now and then, for rooms joined or left elsewhere.
        // A read that took too long has requests still out, whose answers or errors could come
        // late and be taken for a create's or join's: start again on a new connection.
        if let Some((_, started)) = &reload
            && started.elapsed() >= pace.delegate_deadline
        {
            rooms.report(status, report);
            return SocketError::Failed("reading the stored rooms again took too long".into());
        }
        // The signing key sweep's request went unanswered: its answer must not arrive late and
        // be taken for another request's, so start again on a new connection.
        if rooms
            .keys
            .expire(pace.delegate_deadline, key_pace, Instant::now())
        {
            rooms.report(status, report);
            return SocketError::Failed("the delegate did not answer about a signing key".into());
        }
        if reload.is_none()
            && !rooms.keys.busy()
            && reload_due(&mut rooms.outbox.work, last_read.elapsed(), pace)
        {
            let (loader, reads) = Loader::start();
            for read in reads {
                if let Err(e) = socket.send(&delegate::request(&read)) {
                    return e;
                }
            }
            reload = Some((loader, Instant::now()));
        }
        // The signing key sweep comes last, after a read has had its chance to start: it is the
        // lowest priority. It sends one request and waits for its answer before the next.
        let gate = SweepGate {
            task_working: rooms.outbox.work.busy(),
            read_out: reload.is_some(),
            read_pending_or_due: rooms.outbox.work.reload_pending()
                || last_read.elapsed() >= pace.refresh,
            subscribes_pending: !subs.connect_time.is_empty(),
            update_owed: rooms.list.iter().any(|r| {
                rooms
                    .outbox
                    .owes_within(&r.stored.owner_vk, pace.key_owed_cap, Instant::now())
            }),
            complete_read: rooms.complete && !rooms.store_missing,
            settled: last_read.elapsed() >= pace.key_delay,
            writes,
        };
        if let Some(request) = rooms
            .keys
            .next_request(&rooms.list, gate.open(), Instant::now())
            && let Err(e) = socket.send(&request)
        {
            rooms.keys.send_failed(key_pace, Instant::now());
            return e;
        }

        let response = match socket.recv() {
            Ok(r) => Some(r),
            Err(SocketError::Timeout) => None,
            Err(e) => return e,
        };
        // A create's or join's own answer is its alone (`work` takes only what it awaits).
        if let Some(Ok(HostResponse::ContractResponse(r))) = &response
            && rooms.outbox.work.on_contract(r)
        {
            rooms.report(status, report);
            continue;
        }
        match response {
            None => {}
            Some(Ok(HostResponse::ContractResponse(ContractResponse::UpdateResponse {
                key,
                ..
            }))) => {
                changed |= rooms.update_answered(key.id(), Ok(()));
            }
            Some(Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                key,
                state,
                ..
            }))) => {
                subs.answered(key.id());
                changed |= rooms.following(key.id(), true);
                changed |= rooms.merge_bytes(key.id(), state.as_ref());
            }
            // A notification shows the subscription is in place, whatever came of its ask.
            Some(Ok(HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                key,
                update,
            }))) => {
                subs.answered(key.id());
                changed |= rooms.following(key.id(), true);
                changed |= rooms.apply_update(key.id(), &update);
            }
            // The network has no copy. The web UI would seed one with a PUT; this app PUTs only
            // a room it has just created, so it keeps showing the stored copy and asks again.
            Some(Ok(HostResponse::ContractResponse(ContractResponse::NotFound {
                instance_id,
            }))) => {
                subs.failed(instance_id, pace);
                changed |= rooms.following(&instance_id, false);
            }
            Some(Ok(other)) => {
                for answer in delegate::answers(&other) {
                    if rooms.outbox.work.on_delegate(&answer) {
                        changed = true;
                        continue;
                    }
                    if rooms.keys.on_answer(&answer, key_pace, Instant::now()) {
                        continue;
                    }
                    if let Some((loader, _)) = &mut reload {
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
                                subs.keep_only(&rooms.contract_ids());
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
                // River's storage went missing from the node: whatever was waiting on it fails
                // below, and the next connection registers it again before anything else.
                let store_gone = chat_delegate_missing(&e);
                let mut taken_by_work = false;
                // An error that names nothing is a create's or join's only if nothing else
                // could have caused it.
                let alone = subs.pending.is_empty()
                    && reload.is_none()
                    && !rooms
                        .list
                        .iter()
                        .any(|r| rooms.outbox.owes(&r.stored.owner_vk));
                if rooms
                    .outbox
                    .work
                    .on_error(&e, alone, &|id| subs.pending.contains_key(id))
                {
                    changed = true;
                    taken_by_work = true;
                } else if rooms.keys.busy()
                    && (matches!(
                        e.kind(),
                        ErrorKind::RequestError(RequestError::DelegateError(_))
                    ) || (alone && matches!(e.kind(), ErrorKind::Unhandled { .. })))
                {
                    rooms
                        .keys
                        .on_error(&e.to_string(), key_pace, Instant::now());
                } else if let Some((id, cause)) = failed_update(&e) {
                    changed |= rooms.update_answered(&id, Err(cause));
                } else if let Some((id, cause)) =
                    rooms.failed_update_unnamed(&e, &subs, reload.is_some())
                {
                    // Not a reload failure either, so the reload goes on.
                    changed |= rooms.update_answered(&id, Err(cause));
                } else if let Some(id) = failed_contract(&e) {
                    changed |= rooms.subscribe_failed(&mut subs, id, pace);
                } else if reload.is_some() {
                    match delegate_error(&e) {
                        // The store went away: keep the list shown (see the module docs).
                        Ok(()) => eprintln!("River: the stored rooms are gone; keeping the list"),
                        Err(e) => eprintln!("River: reading the stored rooms again failed: {e}"),
                    }
                    reload = None;
                    last_read = Instant::now();
                }
                // A `Missing` the signing key sweep took is a failure of its own (backed off for
                // the whole sweep), and still lands here: it is a store that was there, as a
                // read's `Missing` is, so it registers again by the same rules, and the sweep's
                // wait keeps it from repeating faster than that wait.
                //
                // Only a store that was there, or a create or join the user asked for, is a
                // reason to register again. A store this connection already failed to set up
                // (`store_missing` shown) is not registered again on every re-read: that would
                // re-send the whole WASM each refresh, and wait out `register_wait` each time
                // when the node does not answer. A new connection for any other reason, or the
                // user's own create or join, tries again.
                //
                // A create or join against a node that keeps refusing reconnects on each press.
                // That is accepted: it is the user's doing, and bounded by how often they press
                // Try again. The app's own retries of a store that could not be set up follow
                // `register_retry_first` / `register_retry_max`.
                let retry_due = store_gone
                    && rooms.store_missing
                    && rooms.register_retry_due(Instant::now(), pace);
                // A store this build may no longer register is not a reason to reconnect, and
                // one it may not register yet waits for the check here, on this connection.
                let wants_register = store_gone
                    && !watching.watch.is_stale()
                    && (!rooms.store_missing || taken_by_work || retry_due);
                if wants_register && !watching.settled(pace) {
                    rooms.register_deferred = true;
                    changed = true;
                } else if wants_register {
                    rooms.register_first = true;
                    rooms.report(status, report);
                    return SocketError::Failed(
                        "River's storage went missing from Freenet; registering it again".into(),
                    );
                }
            }
        }
        // An ask that went unanswered for the catch-up time counts as failed: named on screen
        // and asked again later. This holds for every ask, not only the first ones.
        for id in subs.expire(pace.catch_up, pace) {
            changed |= rooms.following(&id, false);
        }
        // "Catching up" ends when every room has answered, or after a while regardless.
        if status == LinkStatus::Loading
            && (subs.pending.is_empty() || connected_at.elapsed() >= pace.catch_up)
        {
            status = LinkStatus::Live;
            changed = true;
        }
        if changed {
            rooms.report(status, report);
        }
    }
}

/// What must hold before the signing key sweep sends a request: it is the lowest priority, so
/// it waits for everything else, and its request is one nothing else may be beside.
struct SweepGate {
    /// A create or join works.
    task_working: bool,
    /// A read of the stored rooms is out.
    read_out: bool,
    /// A read is wanted (a task just finished) or due by the refresh interval.
    read_pending_or_due: bool,
    /// A subscribe asked as the connection was set up is unanswered: an error that names nothing
    /// could be its. Later asks, for rooms that never answer, do not count.
    subscribes_pending: bool,
    /// An update's answer, asked for less than `key_owed_cap` ago, is owed, for the same reason.
    update_owed: bool,
    /// The last read of the stored rooms was complete, and the store is there.
    complete_read: bool,
    /// A few seconds have passed since that read.
    settled: bool,
    /// The app may write: River has not moved on, and the first pointer check is settled.
    writes: bool,
}

impl SweepGate {
    fn open(&self) -> bool {
        !self.task_working
            && !self.read_out
            && !self.read_pending_or_due
            && !self.subscribes_pending
            && !self.update_owed
            && self.complete_read
            && self.settled
            && self.writes
    }
}

/// Whether to read the stored rooms again now: when the refresh interval has passed, or at once
/// when a create or join has just finished, but never while one works. A read started then
/// would put its requests beside the task's, and an error names neither.
fn reload_due(work: &mut super::work::Work, since_read: Duration, pace: &Pace) -> bool {
    !work.busy() && (work.take_reload() || since_read >= pace.refresh)
}

/// The room contract and the node's reason, when an update failed.
fn failed_update(e: &ClientError) -> Option<(ContractInstanceId, String)> {
    match e.kind() {
        ErrorKind::RequestError(RequestError::ContractError(ContractError::Update {
            key,
            cause,
        })) => {
            // The node's own text; kept short for the screen.
            let short: String = cause.chars().take(200).collect();
            let cause = if short.len() < cause.len() {
                format!("{short}…")
            } else {
                short
            };
            Some((*key.id(), cause))
        }
        _ => None,
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
    /// The rooms' notification modes, from the same `rooms_meta` (rooms with none are `All`).
    modes: HashMap<VerifyingKey, NotificationMode>,
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
            modes: HashMap::new(),
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
        let (meta, modes) = match wanted.remove(ROOMS_META_KEY) {
            Some(Some(bytes)) if !bytes.is_empty() => (
                decode_rooms_meta(&bytes).unwrap_or_else(|e| {
                    eprintln!("River: {e}");
                    StoredRoomsMeta::default()
                }),
                decode_modes(&bytes),
            ),
            _ => (StoredRoomsMeta::default(), HashMap::new()),
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
            modes,
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

/// What the node said to registering the chat delegate.
#[derive(Debug, PartialEq, Eq)]
enum Registered {
    Yes,
    Refused(String),
    Unanswered,
}

/// [`register`], with its outcome logged. Only a broken connection stops the session: a refused
/// or unanswered registration leaves the next read to say whether the store is there.
fn register_logged(socket: &mut NodeSocket, pace: &Pace) -> Result<(), SocketError> {
    match register(socket, pace)? {
        Registered::Yes => {}
        Registered::Refused(why) => {
            eprintln!("River: Freenet refused to register River's storage: {why}")
        }
        Registered::Unanswered => {
            eprintln!("River: no answer to registering River's storage; reading the rooms anyway")
        }
    }
    Ok(())
}

/// Register the chat delegate and wait for the node's answer, for up to
/// [`Pace::register_wait`]. Nothing else is out on the connection then (the stored rooms were
/// read, or nothing was sent yet), so the answer is told apart without a request id: the node
/// answers a registration with an empty `DelegateResponse` for the delegate's key (every chat
/// delegate answer carries a message), and a refusal with an error. Since freenet-core 0.2.137
/// (#5727) registrations have their own rate-limit lane, so the `Missing` that led here does
/// not hold this one back.
fn register(socket: &mut NodeSocket, pace: &Pace) -> Result<Registered, SocketError> {
    socket.register_chat_delegate()?;
    let deadline = Instant::now() + pace.register_wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(Registered::Unanswered);
        }
        socket.set_read_timeout(left.max(Duration::from_millis(1)))?;
        match socket.recv() {
            Ok(Ok(HostResponse::DelegateResponse { key, values }))
                if &key == delegate::chat_delegate_key() && values.is_empty() =>
            {
                return Ok(Registered::Yes);
            }
            Ok(Ok(_)) => {}
            // An error that names another delegate is not this registration's answer.
            Ok(Err(e)) if names_another_delegate(&e) => {}
            // Nothing else is out on the connection, so any other error answers it.
            Ok(Err(e)) => return Ok(Registered::Refused(e.to_string())),
            Err(SocketError::Timeout) => {}
            Err(e) => return Err(e),
        }
    }
}

/// Whether an error names a delegate other than River's chat delegate.
fn names_another_delegate(e: &ClientError) -> bool {
    match e.kind() {
        ErrorKind::RequestError(RequestError::DelegateError(
            DelegateError::Missing(key) | DelegateError::RegisterError(key),
        )) => key != delegate::chat_delegate_key(),
        _ => false,
    }
}

/// Whether an error says River's own chat delegate is not on the node.
fn chat_delegate_missing(e: &ClientError) -> bool {
    match e.kind() {
        ErrorKind::RequestError(RequestError::DelegateError(DelegateError::Missing(key))) => {
            key == delegate::chat_delegate_key()
        }
        _ => false,
    }
}

/// `Ok(())` when the error only says the delegate is missing; the error to stop on otherwise.
fn delegate_error(e: &ClientError) -> Result<(), SocketError> {
    if chat_delegate_missing(e) {
        return Ok(());
    }
    let text = e.to_string();
    // The untyped wordings older nodes use for a delegate that is not installed (the web
    // UI's `is_missing_delegate_error`): "delegate <key> not found in store" and
    // "missing delegate <key>", here without regard to case.
    // A typed `Missing` names its delegate and was answered above; only an untyped error is
    // read by its words.
    let typed_missing = matches!(
        e.kind(),
        ErrorKind::RequestError(RequestError::DelegateError(DelegateError::Missing(_)))
    );
    let lower = text.to_lowercase();
    // Either wording, and only for an untyped error.
    if !typed_missing
        && (lower.contains("missing delegate")
            || (lower.contains("delegate") && lower.contains("not found in store")))
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
    /// Each room's notification mode as the store holds it: as last read, and as this app
    /// stored it since. A room with none is `All`. What is shown is this with the picks still
    /// being stored laid over it.
    modes: HashMap<VerifyingKey, NotificationMode>,
    /// Whether the rooms have been read at least once this run.
    loaded: bool,
    store_missing: bool,
    unreadable: usize,
    /// Rooms whose latest from the network could not be applied.
    not_updated: HashSet<VerifyingKey>,
    /// Rooms not subscribed to yet.
    not_following: HashSet<VerifyingKey>,
    /// Whether the last read listed every room: a store, rooms listed, all readable. Only such
    /// a read says a room is gone.
    complete: bool,
    /// What the user sent that the node has not taken yet, and the rooms being created or
    /// joined.
    outbox: Outbox,
    /// River's web app on the node, which invite links open.
    web_app: String,
    /// Invitations made since the last report, by room key.
    invites: Vec<(String, Result<String, String>)>,
    /// The store went missing on the last connection: the next one registers it first.
    register_first: bool,
    /// While the store could not be set up: when the app next registers it by itself, and
    /// the wait after that one. `None` while the store is there.
    register_retry: Option<(Instant, Duration)>,
    /// Keeping the delegate's copy of each room's signing key.
    keys: super::keys::KeySweep,
    /// River has moved on: nothing is written, and the screen says so.
    update_required: bool,
    /// The store needs registering, and waits for the first pointer check to settle.
    register_deferred: bool,
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
            // Still recorded: a store that could not be set up again is not registered again
            // on every re-read (`follow_on`).
            self.store_missing = loaded.store_missing;
            return Vec::new();
        }
        // The rooms stored now are the ones there are: a finished leave has done its work.
        self.outbox.work.rooms_read();
        let mut old: HashMap<VerifyingKey, LiveRoom> = self
            .list
            .drain(..)
            .map(|r| (r.stored.owner_vk, r))
            .collect();
        let mut new_ids = Vec::new();
        let mut by_key: HashMap<VerifyingKey, LiveRoom> = HashMap::new();
        for slot in loaded.slots {
            let vk = slot.owner_vk;
            // A room being left stays off the list (and unsubscribed) while its tombstone
            // is on its way: the read just made may have come first.
            if self.outbox.work.leaving(&vk) {
                continue;
            }
            let room = match old.remove(&vk) {
                Some(mut kept) if same_room(&kept.stored, &slot) => {
                    kept.stored.last_read_message_id = slot.last_read_message_id.clone();
                    newer_self_cache(&mut kept.stored, &slot);
                    if let Err(e) = kept.merge_state(&slot.room_state) {
                        eprintln!("River: the stored copy of a room was not merged: {e}");
                        self.not_updated.insert(vk);
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
                        // Re-keyed: answers owed under the old contract won't come under this.
                        self.outbox.forget_owed(&vk);
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
        self.modes = loaded.modes;
        self.store_missing = loaded.store_missing;
        if !self.store_missing {
            // Found: the next failure starts the retry wait from the beginning.
            self.register_retry = None;
        }
        self.unreadable = loaded.unreadable;
        self.loaded = true;
        // Only a complete read says which rooms are gone. A first read of a run that finds no
        // store or nothing, or a room it couldn't read, must not drop what was left unsent.
        self.complete = !loaded.store_missing && loaded.listed > 0 && loaded.unreadable == 0;
        if self.complete {
            self.outbox.keep_rooms(&shown);
        }
        for room in &mut self.list {
            self.outbox.reconcile(room);
        }
        new_ids
    }

    /// Take what the user did since the last call. True when it changed what shows.
    fn take_commands(&mut self, commands: &Receiver<Command>) -> bool {
        self.outbox.waiting.extend(commands.try_iter());
        if !self.loaded || self.outbox.waiting.is_empty() {
            return false;
        }
        let mut changed = false;
        for command in std::mem::take(&mut self.outbox.waiting) {
            if command.action.is_list_level() {
                changed |= self.list_command(&command.room, command.action);
                continue;
            }
            match self.list.iter().find(|r| r.key() == command.room) {
                Some(room) if command.action == Action::Invite => {
                    let link = super::create::invitation(room, &self.web_app).map(|(_, l)| l);
                    self.invites.push((room.key(), link));
                    changed = true;
                }
                Some(room) => changed |= self.outbox.command(room, command.action),
                // The read so far says nothing about this room (no store, nothing listed, or
                // a slot that couldn't be read): it waits for a read that does.
                None if !self.complete => self.outbox.waiting.push(command),
                // Left, in the browser, before this arrived. An invitation asked for there is
                // said to have failed, so the screen does not wait for it.
                None => {
                    eprintln!("River: a command for a room no longer shown was dropped");
                    if command.action == Action::Invite {
                        self.invites
                            .push((command.room, Err("This room is no longer here.".into())));
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Take every room being left out of the list at once, with what was waiting to be sent to
    /// it, so nothing more is sent, subscribed to or shown for it, and a join of the same room
    /// is not refused as "already here". A leave that fails before the tombstone is stored
    /// asks for the stored rooms to be read again, which puts the room back.
    fn drop_leaving(&mut self) {
        let work = &self.outbox.work;
        let gone: Vec<(VerifyingKey, ContractInstanceId)> = self
            .list
            .iter()
            .filter(|r| work.leaving(&r.stored.owner_vk))
            .map(|r| (r.stored.owner_vk, r.contract_id()))
            .collect();
        for (vk, id) in gone {
            self.list.retain(|r| r.stored.owner_vk != vk);
            self.outbox.purge_room(&vk, id);
            self.not_updated.remove(&vk);
            self.not_following.remove(&vk);
        }
    }

    /// Create, join, or try again or drop one that failed ([`super::work`]). True when it
    /// changed what shows.
    fn list_command(&mut self, room: &str, action: Action) -> bool {
        let shown_owner = || {
            bs58::decode(room)
                .into_vec()
                .ok()
                .and_then(|b| <[u8; 32]>::try_from(b).ok())
                .and_then(|b| VerifyingKey::from_bytes(&b).ok())
                .filter(|vk| self.list.iter().any(|r| &r.stored.owner_vk == vk))
        };
        match action {
            Action::LeaveRoom => {
                let Some(owner) = shown_owner() else {
                    eprintln!("River: leaving a room no longer shown was dropped");
                    return false;
                };
                let name = self
                    .list
                    .iter()
                    .find(|r| r.stored.owner_vk == owner)
                    .map(LiveRoom::name)
                    .unwrap_or_default();
                let identity = self
                    .list
                    .iter()
                    .find(|r| r.stored.owner_vk == owner)
                    .and_then(|r| r.stored.self_verifying_key());
                if !self.outbox.work.leave(owner, name, identity) {
                    return false;
                }
                self.drop_leaving();
                true
            }
            Action::SetNotificationMode { mode } => {
                let owner = shown_owner();
                let Some(owner) = owner else {
                    eprintln!(
                        "River: a notification setting for a room no longer shown was dropped"
                    );
                    return false;
                };
                self.outbox.work.set_mode(owner, mode);
                true
            }
            Action::CreateRoom {
                name,
                nickname,
                private,
            } => {
                self.outbox.work.create(&name, &nickname, private);
                true
            }
            Action::Join { code, nickname } => {
                let shown: HashSet<VerifyingKey> =
                    self.list.iter().map(|r| r.stored.owner_vk).collect();
                self.outbox
                    .work
                    .join(&code.0, &nickname, &|vk| shown.contains(vk));
                true
            }
            Action::RetryWork { id } => {
                let retried = self.outbox.work.retry(id);
                if retried {
                    self.drop_leaving();
                }
                retried
            }
            Action::DismissWork { id } => self.outbox.work.dismiss(id),
            _ => false,
        }
    }

    /// The room an UPDATE failure names some other way than `ContractError::Update`: a
    /// `MissingContract` for a room that owes an answer and has no subscribe waiting for one,
    /// or an error that names nothing (`EmptyRing`, `PeerNotJoined`, `Shutdown`, an operation
    /// error or timeout, as the node reports a failed start) when exactly one room owes an
    /// answer, no subscribe is waiting for one, and no read of the stored rooms nor request of
    /// a create or join is under way (whose requests can fail the same way).
    fn failed_update_unnamed(
        &self,
        e: &ClientError,
        subs: &Subscriptions,
        reloading: bool,
    ) -> Option<(ContractInstanceId, String)> {
        let owing: Vec<ContractInstanceId> = self
            .list
            .iter()
            .filter(|r| self.outbox.owes(&r.stored.owner_vk))
            .map(LiveRoom::contract_id)
            .collect();
        match e.kind() {
            ErrorKind::RequestError(RequestError::ContractError(
                ContractError::MissingContract { key },
            )) if owing.contains(key) && !subs.pending.contains_key(key) => {
                Some((*key, "Freenet doesn't have this room.".into()))
            }
            ErrorKind::EmptyRing
            | ErrorKind::PeerNotJoined
            | ErrorKind::Shutdown
            | ErrorKind::OperationError { .. }
            | ErrorKind::FailedOperation
                if owing.len() == 1
                    && !self.outbox.has_stale()
                    && subs.pending.is_empty()
                    && !reloading
                    && !self.outbox.work.has_out() =>
            {
                Some((owing[0], e.kind().to_string()))
            }
            _ => None,
        }
    }

    /// While disconnected: take what the user did and show it. It is sent on reconnecting.
    fn idle(&mut self, commands: &Receiver<Command>, report: &dyn Fn(RoomsUpdate)) {
        if self.take_commands(commands) {
            self.report(LinkStatus::Reconnecting, report);
        }
    }

    /// Send each room's next update, if it has one and none is out. `Ok(true)` when an entry
    /// failed before it could be sent.
    fn send_updates(&mut self, socket: &mut NodeSocket) -> Result<bool, SocketError> {
        if self.outbox.is_empty() {
            return Ok(false);
        }
        let mut changed = false;
        for i in 0..self.list.len() {
            let room = &self.list[i];
            // A room being joined gets nothing else until the join is answered.
            if self.outbox.work.holds(&room.stored.owner_vk) {
                continue;
            }
            let Some(delta) = self.outbox.next_update(room, Instant::now(), &mut changed) else {
                continue;
            };
            let key = room.stored.contract_key;
            let mut bytes = Vec::new();
            if let Err(e) = ciborium::ser::into_writer(&delta, &mut bytes) {
                // Not expected for River's own types; the entry fails visibly.
                self.outbox.answered(
                    &mut self.list[i],
                    Err(format!("it could not be encoded: {e}")),
                );
                changed = true;
                continue;
            }
            let update = ContractRequest::Update {
                key,
                data: UpdateData::Delta(StateDelta::from(bytes)),
            };
            socket.send(&ClientRequest::ContractOp(update))?;
        }
        Ok(changed)
    }

    /// The node answered the oldest update the room under contract `id` owes an answer
    /// ([`Outbox::answered`]). True when that changed what shows.
    fn update_answered(&mut self, id: &ContractInstanceId, result: Result<(), String>) -> bool {
        // An answer owed by a room that was left, which may be back.
        if self.outbox.take_stale(id) {
            return false;
        }
        let Some(room) = self.list.iter_mut().find(|r| r.contract_id() == *id) else {
            return false;
        };
        let changed = self.outbox.answered(room, result);
        // Taking them may have settled others the room now holds.
        self.outbox.reconcile(room) | changed
    }

    fn contract_ids(&self) -> Vec<ContractInstanceId> {
        self.list.iter().map(LiveRoom::contract_id).collect()
    }

    fn by_contract(&mut self, id: &ContractInstanceId) -> Option<&mut LiveRoom> {
        self.list.iter_mut().find(|r| r.contract_id() == *id)
    }

    /// A subscribe GET that failed: the room stays on its copy and is asked again. A room not
    /// shown (the room of a join that gave up while its GET was out) is not asked for: that
    /// would subscribe to a room the user is not in. True when it changed what shows.
    fn subscribe_failed(
        &mut self,
        subs: &mut Subscriptions,
        id: ContractInstanceId,
        pace: &Pace,
    ) -> bool {
        if self.by_contract(&id).is_none() {
            return false;
        }
        subs.failed(id, pace);
        self.following(&id, false)
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
        let Some(room) = self.list.iter_mut().find(|r| r.contract_id() == *id) else {
            return false;
        };
        let result = ciborium::de::from_reader::<ChatRoomStateV1, _>(bytes)
            .map_err(|e| format!("unreadable state: {e}"))
            .and_then(|incoming| room.merge_state(&incoming));
        let sent = self.outbox.reconcile(room);
        self.outcome(id, result) | sent
    }

    fn apply_update(&mut self, id: &ContractInstanceId, update: &UpdateData<'_>) -> bool {
        match update {
            UpdateData::State(state) => self.merge_bytes(id, state.as_ref()),
            UpdateData::StateAndDelta { state, .. } => self.merge_bytes(id, state.as_ref()),
            UpdateData::Delta(delta) => {
                let Some(room) = self.list.iter_mut().find(|r| r.contract_id() == *id) else {
                    return false;
                };
                let result = ciborium::de::from_reader::<ChatRoomStateV1Delta, _>(delta.as_ref())
                    .map_err(|e| format!("unreadable delta: {e}"))
                    .and_then(|delta| room.apply_delta(delta));
                let sent = self.outbox.reconcile(room);
                self.outcome(id, result) | sent
            }
            // Related-contract updates, and variants newer than this stdlib.
            _ => false,
        }
    }

    fn views(&self) -> Vec<Room> {
        self.list
            .iter()
            .map(|r| {
                let mut view = self.outbox.view(r, &local_minutes);
                if self.update_required {
                    view.send_block = Some(UPDATE_REQUIRED_SEND.to_owned());
                }
                view
            })
            .collect()
    }

    fn names(&self, set: &HashSet<VerifyingKey>) -> Vec<String> {
        self.list
            .iter()
            .filter(|r| set.contains(&r.stored.owner_vk))
            .map(LiveRoom::name)
            .collect()
    }

    /// Whether the app's own retry of registering a store that could not be set up is due now.
    /// Taking it doubles the next wait, up to [`Pace::register_retry_max`]. The first wait
    /// starts at the first re-read that finds the store still missing.
    fn register_retry_due(&mut self, now: Instant, pace: &Pace) -> bool {
        match self.register_retry {
            None => {
                self.register_retry =
                    Some((now + pace.register_retry_first, pace.register_retry_first));
                false
            }
            Some((at, _)) if now < at => false,
            Some((_, wait)) => {
                let next = wait.saturating_mul(2).min(pace.register_retry_max);
                self.register_retry = Some((now + next, next));
                true
            }
        }
    }

    fn issues(&self) -> Issues {
        Issues {
            store_missing: self.store_missing,
            not_updated: self.names(&self.not_updated),
            not_following: self.names(&self.not_following),
            unreadable: self.unreadable,
            update_required: self.update_required,
            // Also across the reconnect that registers, so the screen does not flash "could not
            // set up" in between.
            setting_up: self.register_deferred
                || (self.register_first && self.store_missing && !self.update_required),
        }
    }

    fn report(&mut self, status: LinkStatus, report: &dyn Fn(RoomsUpdate)) {
        // A pick that was stored is the store's now; one that failed never was.
        let mut settled = Vec::new();
        for (vk, mode, stored) in self.outbox.work.settle_modes() {
            if stored {
                self.modes.insert(vk, mode);
            }
            settled.push(bs58::encode(vk.to_bytes()).into_string());
        }
        // What is shown: the store's modes, with the picks still being stored over them.
        let mut shown = self.modes.clone();
        shown.extend(self.outbox.work.pending_modes());
        report(RoomsUpdate::Rooms {
            rooms: self.views(),
            open: self.open.clone(),
            modes: shown
                .iter()
                .map(|(vk, m)| (bs58::encode(vk.to_bytes()).into_string(), *m))
                .collect(),
            settled,
            status,
            now: local_minutes(SystemTime::now()),
            issues: self.issues(),
            work: self.outbox.work.views(),
        });
        for (room, link) in std::mem::take(&mut self.invites) {
            report(RoomsUpdate::Invite { room, link });
        }
        // A finished task is shown once its room is in the list (so the screen can open it),
        // then it is done with.
        let shown: HashSet<VerifyingKey> = self.list.iter().map(|r| r.stored.owner_vk).collect();
        self.outbox.work.forget_done(&|vk| shown.contains(vk));
    }

    /// The connection is gone: keep showing the rooms if there are any, and say so.
    fn lost(&self, why: &str, report: &dyn Fn(RoomsUpdate)) {
        eprintln!("River: lost the connection to Freenet: {why}");
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
}

use river_core::room_state::member::MemberId as SelfId;

/// The record of yours cached for re-adding you, and the nickname beside it. This app writes
/// them in memory when a nickname of yours settles and never into the store; the web UI writes
/// them into the store. A read keeps whichever is newer by the record's version: the stored
/// one when the web UI changed it since (its record is above ours), else ours, which the store
/// has not caught up with. Not a reason to take the stored room whole: that would drop the
/// write, and rebuild the room on every read after a rename.
fn newer_self_cache(kept: &mut StoredRoom, slot: &StoredRoom) {
    // Only a record of ours counts, as when the version to sign is chosen.
    let me = |r: &StoredRoom| r.self_verifying_key().map(|vk| SelfId::from(&vk));
    let version = |r: &StoredRoom| {
        r.self_member_info
            .as_ref()
            .filter(|i| Some(i.member_info.member_id) == me(r))
            .map(|i| i.member_info.version)
    };
    if slot
        .self_member_info
        .as_ref()
        .is_some_and(|i| Some(i.member_info.member_id) != me(slot))
    {
        return;
    }
    let adopt = match (version(kept), version(slot)) {
        (Some(ours), Some(theirs)) => theirs > ours,
        (Some(_), None) => false,
        // Nothing of ours: whatever the store holds, as before.
        (None, _) => true,
    };
    if adopt {
        kept.self_member_info = slot.self_member_info.clone();
        kept.self_nickname = slot.self_nickname.clone();
    }
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
