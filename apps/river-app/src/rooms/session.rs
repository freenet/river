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
//! - **Reading rooms again.** While connected, the open room is read again when the node has said
//!   nothing about it for [`Pace::resync_idle`], every room not heard about lately when the window
//!   comes to the front, and a room whose update could not be merged after a backoff
//!   ([`super::resync`] has the rules and what bounds them). Such a read never reconnects.
//! - **Earlier River releases.** Once a run, after the first read of the stored rooms, the
//!   rooms earlier chat delegates hold and the current one does not are carried over
//!   ([`super::carry`]), as a task. A room stored under an earlier room-contract key is followed
//!   under the current one, and moved there when the network has no state under it
//!   ([`super::rekey`]).
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
use river_core::room_state::member::MemberId;
use river_core::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};
use river_core::stored_rooms::{
    ROOMS_META_KEY, StoredRoom, StoredRoomSlot, StoredRoomsMeta, decode_room_slot,
    decode_rooms_meta, parse_room_storage_key, room_storage_key,
};

use super::delegate::{self, Read};
use super::dm_archive::{self, ArchiveOp};
use super::dm_cache::{self, DmRead, OutboundCache};
use super::dm_save::DmSaver;
use super::dm_store::{DmOp, EntryKey};
use super::generation::{
    CheckPace, UPDATE_REQUIRED, UPDATE_REQUIRED_PURGE, UPDATE_REQUIRED_SEND, UPDATE_REQUIRED_WORK,
    Watch,
};
use super::grant::{GrantOutcome, GrantPacing, GrantToken, NodeHttp, request_grant_with};
use super::modes::{NotificationMode, decode_modes};
use super::outbox::{Action, Command, Outbox};
use super::resync::{ResyncPace, Resyncs};
use super::room::{LiveRoom, NetworkMerge, local_minutes};
use super::socket::{NodeSocket, SocketError, TokenVerdict, token_verdict};
use super::token::TokenStore;
use super::work::WorkView;
pub use crate::chat::LinkStatus;
use crate::chat::Room;
use crate::chat::dm_model::DmSnapshot;
use river_core::room_state::direct_messages::MAX_DM_MESSAGES_PER_PAIR;

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
    /// How long the open room may go with nothing from the node about it (no notification, no
    /// answer to a read) before River reads it again, and so also the most often it is read
    /// again for that reason. The web UI's watchdog waits the same 60 seconds
    /// (`LIVENESS_IDLE_PROBE_MS`) ([`super::resync`]).
    pub resync_idle: Duration,
    /// When the window comes to the front, every room with nothing from the node for at least
    /// this long is read again ([`super::resync`]).
    pub resync_focus: Duration,
    /// Coming to the front spreads its reads over up to this long, at random
    /// ([`super::resync`]).
    pub resync_spread: Duration,
    /// How long such a read may go unanswered before River stops waiting for it. It frees the
    /// room for the next read, a whole `resync_idle` later, and the requests a read holds back;
    /// it never reconnects. Past one attempt of the node's (freenet-core `OPERATION_TTL`, 60 s
    /// per attempt), so a GET that fails at its first attempt is still out when its error comes
    /// ([`super::resync`]).
    pub resync_wait: Duration,
    /// How long a read given up on is kept as late: it no longer holds anything back, but while
    /// any read is late an error that could be its own is claimed by nobody. Past the node's
    /// limit for every attempt of a GET (four attempts of 60 s, freenet-core `OPERATION_TTL`
    /// and `MAX_RETRIES`), counted from when the read was asked ([`super::resync`]).
    pub resync_late: Duration,
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

impl Pace {
    fn resync(&self) -> ResyncPace {
        ResyncPace {
            idle: self.resync_idle,
            focus: self.resync_focus,
            spread: self.resync_spread,
            heal_first: self.resubscribe_first,
            heal_max: self.resubscribe_max,
        }
    }
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
            resync_idle: Duration::from_secs(60),
            resync_focus: Duration::from_secs(60),
            resync_spread: Duration::from_secs(10),
            resync_wait: Duration::from_secs(120),
            resync_late: Duration::from_secs(300),
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
    /// The direct-message threads of every room, sent after `Rooms` whenever they changed.
    /// Read only.
    Dms(DmSnapshot),
    /// What became of a direct message the user asked to send ([`Action::SendDm`]): queued, or
    /// why not. Said once per ticket. The words never quote the message.
    DmSend {
        room: String,
        peer: String,
        ticket: u64,
        result: Result<(), String>,
    },
    /// What became of a deletion the user asked for ([`Action::PurgeDm`]): queued, or why not.
    /// Said once per ticket. The words never name a message.
    DmPurge {
        room: String,
        peer: String,
        ticket: u64,
        result: Result<(), String>,
    },
    /// What became of an invitation the user asked to send ([`Action::SendInviteDm`]) or accept
    /// ([`Action::AcceptInviteDm`]): queued or started, or why not. Said once per ticket. The
    /// words never quote a note and no invitation is in them.
    DmInvite {
        room: String,
        peer: String,
        ticket: u64,
        result: Result<(), String>,
    },
    /// The connection's state changed while rooms are shown.
    Status(LinkStatus),
    /// The invitation the user asked for in the room keyed `room`: its link, or why there is
    /// none.
    Invite {
        room: String,
        link: Result<String, String>,
    },
    /// The identity export the user asked for in the room keyed `room`: its token, or why
    /// there is none. The token is the user's signing key.
    Export {
        room: String,
        token: Result<String, String>,
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
            RoomsUpdate::Dms(d) => f.debug_tuple("Dms").field(d).finish(),
            RoomsUpdate::DmSend {
                room,
                peer,
                ticket,
                result,
            } => f
                .debug_struct("DmSend")
                .field("room", room)
                .field("peer", peer)
                .field("ticket", ticket)
                .field("result", result)
                .finish(),
            RoomsUpdate::DmPurge {
                room,
                peer,
                ticket,
                result,
            } => f
                .debug_struct("DmPurge")
                .field("room", room)
                .field("peer", peer)
                .field("ticket", ticket)
                .field("result", result)
                .finish(),
            RoomsUpdate::DmInvite {
                room,
                peer,
                ticket,
                result,
            } => f
                .debug_struct("DmInvite")
                .field("room", room)
                .field("peer", peer)
                .field("ticket", ticket)
                .field("result", result)
                .finish(),
            RoomsUpdate::Status(s) => f.debug_tuple("Status").field(s).finish(),
            RoomsUpdate::Invite { room, link } => f
                .debug_struct("Invite")
                .field("room", room)
                .field("link", &link.as_ref().map(|_| "<hidden>"))
                .finish(),
            RoomsUpdate::Export { room, token } => f
                .debug_struct("Export")
                .field("room", room)
                .field("token", &token.as_ref().map(|_| "<hidden>"))
                .finish(),
        }
    }
}

/// Why the node would not honour River's permission on this connection, and what to do. The node
/// honours it only on a direct loopback connection with a loopback `Host`.
fn unusable_message(authority: &str) -> String {
    let on_this_computer = authority == crate::address::DEFAULT_AUTHORITY
        || crate::address::NodeAddress::parse(authority).is_ok_and(|a| a.is_loopback());
    if on_this_computer {
        format!(
            "Freenet won't let River use its permission over this connection. River has to \
             reach Freenet directly on this computer, at {authority}."
        )
    } else {
        format!(
            "Freenet won't let River use its permission over a connection to {authority}: it \
             honours it only on a direct connection from the same computer. To use a node on \
             another machine, reach it through an SSH tunnel and give River the tunnel's \
             address on this computer, such as localhost:7509."
        )
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
                            crate::diag!("River: {}", crate::diag::scrub(&e));
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
                return Access::Refused(unusable_message(authority));
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

    /// Ask again for `id` as soon as nothing else is out for it (by [`Self::ask_due`]): the
    /// network has a copy now (the room was moved to its current contract key).
    fn due_now(&mut self, id: ContractInstanceId) {
        let attempts = self.retry.get(&id).map_or(0, |(n, _)| *n);
        self.retry.insert(id, (attempts, Instant::now()));
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

    /// Ask again for every room whose wait is over, except one with a read out: the node's answer
    /// to that read could not be told from the subscribe's ([`super::resync`]).
    fn ask_due(
        &mut self,
        socket: &mut NodeSocket,
        reading: &dyn Fn(&ContractInstanceId) -> bool,
    ) -> Result<(), SocketError> {
        let now = Instant::now();
        let due: Vec<ContractInstanceId> = self
            .retry
            .iter()
            .filter(|(id, (_, at))| *at <= now && !self.pending.contains_key(*id) && !reading(id))
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
    // A read that was out went with the connection: its stamp is not the next one's.
    rooms.dm_read_began = None;
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
    // Once a run, with River's storage there: carry over the rooms earlier River releases stored
    // in their own chat delegates (freenet/river#889). After the read, so it knows which rooms
    // the store holds; it works as a task, so it waits for the write gate like any other.
    if !loaded.store_missing && !std::mem::replace(&mut rooms.carry_started, true) {
        let current = loaded.listed_rooms.iter().map(room_storage_key);
        let empty = loaded.slots.is_empty();
        rooms.outbox.work.carry(super::carry::Carry::new(
            super::carry::predecessors().to_vec(),
            current,
            empty,
        ));
    }
    rooms.replace(loaded);
    rooms.update_required = watching.watch.is_stale();
    // Every room is subscribed afresh on every connection: subscriptions belong to it.
    let mut subs = Subscriptions::default();
    for id in rooms.contract_ids() {
        if let Err(e) = subs.ask_at_connect(socket, id) {
            return e;
        }
    }
    // Reading rooms again belongs to the connection too, and every room was just read: a
    // focus that came meanwhile has nothing left to do.
    let mut resyncs = Resyncs::new(Instant::now());
    rooms.resync_wanted = false;
    rooms.merge_results.clear();
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
        if let Err(e) = subs.ask_due(socket, &|id| resyncs.is_out(id)) {
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
            return SocketError::Failed(if rooms.outbox.work.expired_dm() {
                "reconnecting after saving sent messages went unanswered".into()
            } else {
                "reconnecting to finish creating or joining a room".into()
            });
        }
        // Moves of rooms to their current contract key: follow a moved room there at once,
        // and start the moves whose wait is over.
        for id in rooms.seed_ends(pace, Instant::now()) {
            subs.due_now(id);
            changed = true;
        }
        if writes {
            rooms.seeds_due(Instant::now());
        }
        subs.keep_only(&rooms.contract_ids());
        rooms.archive_revival(unix_secs());
        rooms.dm_save_tick(writes, Instant::now());
        rooms.own_purges_tick();
        if writes {
            match rooms.send_updates(socket) {
                Ok(failed) => changed |= failed,
                Err(e) => return e,
            }
        }
        // The create or join under way sends its next request, unless the stored rooms are
        // being read, a signing key request or a room read is out: their answers and its are
        // told apart by kind, but an error names neither.
        if writes
            && let Some(mut request) = rooms.outbox.work.next_request(
                reload.is_some() || rooms.keys.busy() || resyncs.any_out(),
                Instant::now(),
            )
        {
            let sent = socket.send(&request);
            // A store of the sent-message key holds what the user wrote: not kept after it is out.
            delegate::wipe_request(&mut request);
            if let Err(e) = sent {
                return e;
            }
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
        // Not beside a room read either: an error naming nothing could be either's, and taken
        // for the read of the stored rooms it would abort it ([`super::resync`]). The wait is
        // at most `resync_wait`, and only on a node that answers no GET; this read runs in the
        // background. The same holds for the create or join and the signing key sweep.
        if reload.is_none()
            && !rooms.keys.busy()
            && !resyncs.any_out()
            && reload_due(&mut rooms.outbox.work, last_read.elapsed(), pace)
        {
            let (loader, reads) = Loader::start();
            for read in reads {
                if let Err(e) = socket.send(&delegate::request(&read)) {
                    return e;
                }
            }
            rooms.dm_read_began = Some(rooms.dm_cache.save_seq());
            reload = Some((loader, Instant::now()));
        }
        // Rooms read again (freenet/river#971; [`super::resync`]). Not beside a read of the
        // stored rooms, a signing key request or a create's or join's request: an error that
        // names nothing could then be either's.
        if std::mem::take(&mut rooms.resync_wanted) {
            resyncs.focus(&rooms.contract_ids(), Instant::now(), &pace.resync());
        }
        resyncs.expire(pace.resync_wait, pace.resync_late, Instant::now());
        if reload.is_none() && !rooms.keys.busy() && !rooms.outbox.work.has_out() {
            let viewing = rooms.viewing_contract();
            let ids = rooms.contract_ids();
            let blocked = |id: &ContractInstanceId| {
                subs.pending.contains_key(id)
                    || subs.retry.contains_key(id)
                    || rooms.blocks_read(id)
            };
            for id in resyncs.due(viewing, &ids, &blocked, Instant::now(), &pace.resync()) {
                let get = ContractRequest::Get {
                    key: id,
                    return_contract_code: false,
                    subscribe: false,
                    blocking_subscribe: false,
                };
                if let Err(e) = socket.send(&ClientRequest::ContractOp(get)) {
                    return e;
                }
            }
        }
        // The signing key sweep comes last, after a read has had its chance to start: it is the
        // lowest priority. It sends one request and waits for its answer before the next.
        let gate = SweepGate {
            task_working: rooms.outbox.work.busy(),
            // A room read's error could name nothing, as the sweep's can.
            read_out: reload.is_some() || resyncs.any_out(),
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
            && rooms
                .outbox
                .work
                .on_contract_beside(r, &|id| subs.pending.contains_key(id) || resyncs.is_out(id))
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
                // A read's answer says nothing about the subscription. One for a late read,
                // with no subscribe or other read out for its room, ends that read.
                if !subs.pending.contains_key(key.id()) && !resyncs.is_out(key.id()) {
                    resyncs.late_done(key.id());
                }
                if !resyncs.answered(key.id(), Instant::now()) {
                    subs.answered(key.id());
                    changed |= rooms.following(key.id(), true);
                }
                changed |= rooms.merge_bytes(key.id(), state.as_ref());
            }
            // A notification shows the subscription is in place, whatever came of its ask.
            Some(Ok(HostResponse::ContractResponse(ContractResponse::UpdateNotification {
                key,
                update,
            }))) => {
                subs.answered(key.id());
                resyncs.heard(key.id(), Instant::now());
                changed |= rooms.following(key.id(), true);
                changed |= rooms.apply_update(key.id(), &update);
            }
            // The network has no copy. The web UI would seed one with a PUT; this app PUTs only
            // a room it has just created, or one moved across a room-contract re-key from an
            // earlier key that holds it ([`super::rekey`]), so it keeps showing the stored copy
            // and asks again.
            Some(Ok(HostResponse::ContractResponse(ContractResponse::NotFound {
                instance_id,
            }))) if (resyncs.is_out(&instance_id) || resyncs.is_late(&instance_id))
                && !subs.pending.contains_key(&instance_id) =>
            {
                // A read found nothing (the node also says so for a GET whose attempts ran out,
                // so this is how a read given up on usually ends): the subscription is not in
                // question, and the next read comes by the usual rules.
                // With a read out too, it is either's: the late one stays late.
                if !resyncs.is_out(&instance_id) {
                    resyncs.late_done(&instance_id);
                }
                resyncs.failed(&instance_id);
            }
            Some(Ok(HostResponse::ContractResponse(ContractResponse::NotFound {
                instance_id,
            }))) => {
                subs.failed(instance_id, pace);
                changed |= rooms.following(&instance_id, false);
                // After a room-contract re-key the current key may have no state yet: follow
                // the room under its earlier key, and move it ([`super::rekey`]).
                if let Some(earlier) = rooms.seed_if_rekeyed(&instance_id) {
                    subs.due_now(earlier);
                    subs.keep_only(&rooms.contract_ids());
                    changed = true;
                }
            }
            // An earlier chat delegate's answer, for carrying rooms over: its own key names it.
            Some(Ok(HostResponse::DelegateResponse { key, mut values }))
                if &key != delegate::chat_delegate_key() =>
            {
                let mut response = HostResponse::DelegateResponse {
                    key,
                    values: std::mem::take(&mut values),
                };
                if let Some((from, mut answers)) = super::carry::answers_from(&mut response) {
                    changed |= rooms.outbox.work.on_predecessor(&from, &answers);
                    // A room's slot holds the user's signing key for it.
                    delegate::wipe_answers(&mut answers);
                }
            }
            Some(Ok(mut other)) => {
                let answers = delegate::answers(&other);
                delegate::wipe_response(&mut other);
                for mut answer in answers {
                    if rooms.outbox.work.on_delegate(&answer) {
                        changed = true;
                        // A save's read holds the stored sent messages.
                        delegate::wipe_answers(std::slice::from_mut(&mut answer));
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
                                // No room read is out here (none starts beside a read of the
                                // stored rooms, nor this beside one), so a subscribe asked now
                                // is never beside a read of the same room.
                                debug_assert!(!resyncs.any_out());
                                for id in rooms.replace(*loaded) {
                                    if let Err(e) = subs.ask(socket, id) {
                                        return e;
                                    }
                                }
                                subs.keep_only(&rooms.contract_ids());
                                resyncs.keep_only(&rooms.contract_ids());
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
                // Whether a delegate error could be the signing key sweep's or the stored rooms
                // read's, rather than a task's read of an earlier delegate.
                rooms
                    .outbox
                    .work
                    .set_delegate_shared(rooms.keys.busy() || reload.is_some());
                let mut taken_by_work = false;
                // An error that names nothing is a create's or join's only if nothing else
                // could have caused it.
                let alone = other_out(&subs, &resyncs, reload.is_some(), rooms).none();
                // A read given up on may still fail (the node retries a GET, each attempt up to
                // a minute): an error naming nothing, or naming its room with no other request
                // out for that room, may be that read's, so it is claimed by nobody. What else
                // it could have been runs to its own deadline instead.
                let late_reads = (names_nothing(&e)
                    && (resyncs.any_late() || rooms.outbox.work.has_late()))
                    || failed_contract(&e).is_some_and(|id| {
                        resyncs.is_late(&id)
                            && !resyncs.is_out(&id)
                            && !subs.pending.contains_key(&id)
                    });
                if late_reads {
                    crate::diag!(
                        "River: an error that may be a read given up on, taken by none: {}",
                        crate::diag::scrub(&e)
                    );
                    // One that names the late read's room is that read's: it is over.
                    if let Some(id) = failed_contract(&e) {
                        resyncs.late_done(&id);
                    }
                } else if rooms.outbox.work.on_error(&e, alone, &|id| {
                    subs.pending.contains_key(id) || resyncs.is_out(id)
                }) {
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
                } else if let Some((id, cause)) = rooms
                    .failed_update_unnamed(&e, &subs, reload.is_some() || resyncs.any_out())
                    // An UPDATE goes out for a room whose read is out (the user's message is
                    // not held for a read), so its `MissingContract` could be the read's: it
                    // is taken for the read below. A real UPDATE failure then shows when its
                    // answer is given up on (`update_wait`), never a read's as the message's.
                    .filter(|(id, _)| !resyncs.is_out(id))
                {
                    // Not a reload failure either, so the reload goes on.
                    changed |= rooms.update_answered(&id, Err(cause));
                } else if let Some(id) = rooms.nameless_read(&e, &subs, &resyncs, reload.is_some())
                {
                    // It frees the room for the next read now, instead of after `resync_wait`.
                    crate::diag!(
                        "River: reading a room again failed: {}",
                        crate::diag::scrub(&e)
                    );
                    resyncs.failed(&id);
                } else if let Some(id) = failed_contract(&e)
                    && resyncs.is_out(&id)
                    && !subs.pending.contains_key(&id)
                {
                    // A read of the room failed: the subscription is not in question.
                    crate::diag!(
                        "River: reading a room again failed: {}",
                        crate::diag::scrub(&e)
                    );
                    resyncs.failed(&id);
                } else if let Some(id) = failed_contract(&e) {
                    changed |= rooms.subscribe_failed(&mut subs, id, pace);
                } else if reload.is_some() {
                    match delegate_error(&e) {
                        // The store went away: keep the list shown (see the module docs).
                        Ok(()) => {
                            crate::diag!("River: the stored rooms are gone; keeping the list")
                        }
                        Err(e) => crate::diag!(
                            "River: reading the stored rooms again failed: {}",
                            crate::diag::scrub(&e)
                        ),
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
        // Merges that failed are read again, after a while ([`super::resync`]).
        for (id, result) in rooms.merge_results.drain(..) {
            resyncs.merged(id, &result, Instant::now(), &pace.resync());
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

/// How many times a run a moved room's earlier keys that did not answer, or answered NotFound
/// while a merge is owed from them, are read again.
const MAX_FOLD_RETRIES: u32 = 5;

/// What else may be out on the connection when an error that names nothing arrives.
#[derive(Clone, Copy, Debug, Default)]
struct OtherOut {
    subscribing: bool,
    reading_rooms: bool,
    reading_store: bool,
    key_request: bool,
    update_owed: bool,
}

/// What else is out on the connection now.
fn other_out(
    subs: &Subscriptions,
    resyncs: &Resyncs,
    reading_store: bool,
    rooms: &Rooms,
) -> OtherOut {
    OtherOut {
        subscribing: !subs.pending.is_empty(),
        reading_rooms: resyncs.any_out(),
        reading_store,
        key_request: rooms.keys.busy(),
        update_owed: rooms
            .list
            .iter()
            .any(|r| rooms.outbox.owes(&r.stored.owner_vk)),
    }
}

impl OtherOut {
    /// Whether nothing but a create's or join's request can have caused an error that names
    /// nothing: no subscribe, room read, read of the stored rooms or signing key request is
    /// out, and no room owes an update's answer.
    fn none(self) -> bool {
        !self.subscribing
            && !self.reading_rooms
            && !self.reading_store
            && !self.key_request
            && !self.update_owed
    }
}

/// Whether the node's error names no request: how it reports one that failed to start
/// (`EmptyRing`, `PeerNotJoined`, `Shutdown`, an operation error or failure).
fn names_nothing(e: &ClientError) -> bool {
    matches!(
        e.kind(),
        ErrorKind::EmptyRing
            | ErrorKind::PeerNotJoined
            | ErrorKind::Shutdown
            | ErrorKind::OperationError { .. }
            | ErrorKind::FailedOperation
    )
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
    /// Those keys' rooms, by owner (tombstones included).
    listed_rooms: Vec<VerifyingKey>,
    /// The earlier room-contract keys each room's slot still owes a merge from (the web UI's
    /// `rekey_merge.owed`), by owner.
    owed: HashMap<VerifyingKey, Vec<freenet_stdlib::prelude::ContractKey>>,
    /// Stored rooms that could not be decoded.
    unreadable: usize,
    /// What `outbound_dms` held (read here; written by [`super::dm_save`]).
    dms: DmRead,
}

impl Loaded {
    fn missing() -> Self {
        Loaded {
            slots: Vec::new(),
            meta: StoredRoomsMeta::default(),
            modes: HashMap::new(),
            store_missing: true,
            listed: 0,
            listed_rooms: Vec::new(),
            owed: HashMap::new(),
            unreadable: 0,
            dms: DmRead::NotListed,
        }
    }
}

/// Reading the stored rooms from the chat delegate, one answer at a time: list the keys, then
/// get each room and the list's view preferences. Answers are matched by storage key, as the
/// web UI matches them, so their order does not matter.
/// The keys a read waits for: each is `None` until answered, then the answer's value.
type Wanted = HashMap<Vec<u8>, Option<Option<Vec<u8>>>>;

struct Loader {
    /// `None` until the key list has arrived.
    /// By storage key: `None` until the key's answer arrives, then the answer's value (itself
    /// `None` for a key the delegate has no value for).
    wanted: Option<Wanted>,
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
                let mut wanted: Wanted = plan
                    .rooms
                    .iter()
                    .map(|vk| (room_storage_key(vk), None))
                    .collect();
                if plan.has_meta {
                    wanted.insert(ROOMS_META_KEY.to_vec(), None);
                }
                if plan.has_dms {
                    wanted.insert(dm_cache::KEY.to_vec(), None);
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
                    *slot = Some(value);
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
            Some(Some(Some(bytes))) if !bytes.is_empty() => (
                decode_rooms_meta(&bytes).unwrap_or_else(|e| {
                    crate::diag!("River: {}", crate::diag::scrub(&e));
                    StoredRoomsMeta::default()
                }),
                decode_modes(&bytes),
            ),
            _ => (StoredRoomsMeta::default(), HashMap::new()),
        };
        // Listed but with no value, or a value that does not decode, is unreadable: only a key
        // the list did not have is "not listed".
        let dms = match wanted.remove(dm_cache::KEY) {
            Some(Some(Some(bytes))) => match OutboundCache::from_bytes(&bytes) {
                Ok(cache) => DmRead::Read(Box::new(cache)),
                Err(e) => {
                    crate::diag!("River: {}", crate::diag::scrub(&e));
                    DmRead::Unreadable
                }
            },
            Some(_) => DmRead::Unreadable,
            None => DmRead::NotListed,
        };
        let mut slots = Vec::new();
        let mut owed = HashMap::new();
        let mut unreadable = 0;
        for vk in &self.rooms {
            let Some(Some(Some(bytes))) = wanted.remove(&room_storage_key(vk)) else {
                continue;
            };
            if bytes.is_empty() {
                continue;
            }
            match decode_room_slot(&bytes) {
                Ok(StoredRoomSlot::Present(room)) => {
                    let keys = super::rekey::owed_keys(&bytes);
                    if !keys.is_empty() {
                        owed.insert(room.owner_vk, keys);
                    }
                    slots.push(*room)
                }
                Ok(StoredRoomSlot::Tombstone) => {}
                // One unreadable room must not hide the others; the screen counts it.
                Err(e) => {
                    crate::diag!("River: skipping a stored room: {}", crate::diag::scrub(&e));
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
            listed_rooms: self.rooms.clone(),
            owed,
            unreadable,
            dms,
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
            crate::diag!(
                "River: Freenet refused to register River's storage: {}",
                crate::diag::scrub(&why)
            )
        }
        Registered::Unanswered => {
            crate::diag!(
                "River: no answer to registering River's storage; reading the rooms anyway"
            )
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

/// What a `ListResponse` says to read: the per-room keys, and whether `rooms_meta` and
/// `outbound_dms` exist.
#[derive(Debug, PartialEq)]
struct Plan {
    rooms: Vec<VerifyingKey>,
    has_meta: bool,
    has_dms: bool,
}

fn plan<'a>(keys: impl Iterator<Item = &'a [u8]>) -> Plan {
    let mut plan = Plan {
        rooms: Vec::new(),
        has_meta: false,
        has_dms: false,
    };
    for key in keys {
        if let Some(vk) = parse_room_storage_key(key) {
            if !plan.rooms.contains(&vk) {
                plan.rooms.push(vk);
            }
        } else if key == ROOMS_META_KEY {
            plan.has_meta = true;
        } else if key == dm_cache::KEY {
            plan.has_dms = true;
        }
    }
    plan
}

/// A room's threads as last built, with the key they were built under. The one copy of their
/// decrypted text in the background work: the window's snapshot shares it.
struct DmMemo {
    key: super::dm::DmKey,
    threads: Vec<std::sync::Arc<crate::chat::dm_model::DmThread>>,
    truncated: bool,
    /// How many times the room's threads were built, for tests.
    builds: usize,
}

/// The fewest seconds between two reads of the stored rooms asked for by sends into threads this
/// app does not hold as archived.
const REREAD_EVERY: Duration = Duration::from_secs(30);

/// The time in unix seconds.
fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What the window was last sent as direct-message threads ([`Rooms::dm_sent`]): each room's
/// threads' key and a digest of its unsent direct messages, whether the sent-text cache had been
/// read, which rooms were listed, and the archive list's revision.
type DmSent = (
    Vec<(VerifyingKey, super::dm::DmKey, u64)>,
    bool,
    Vec<String>,
    u64,
);

/// The answer to one [`Action::SendDm`].
struct DmSendResult {
    room: String,
    peer: String,
    ticket: u64,
    result: Result<(), String>,
}

/// The answer to one [`Action::SendInviteDm`] or [`Action::AcceptInviteDm`].
struct DmInviteResult {
    room: String,
    peer: String,
    ticket: u64,
    result: Result<(), String>,
}

/// What a deletion says when the room has not been read from the network yet.
const NOT_LOADED_PURGE: &str = "River hasn't loaded this room's latest messages from Freenet \
     yet, so it can't delete safely. Try again in a moment.";

/// The answer to one [`Action::PurgeDm`].
struct DmPurgeResult {
    room: String,
    peer: String,
    ticket: u64,
    result: Result<(), String>,
}

/// A digest of what a room's unsent direct messages and deletions show: which there are and how
/// each stands. Zero for none.
fn pending_digest(rows: &[super::outbox::DmRow], purges: &[super::outbox::PurgeRow]) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    if rows.is_empty() && purges.is_empty() {
        return 0;
    }
    let mut h = DefaultHasher::new();
    for p in purges {
        p.peer.0.0.hash(&mut h);
        p.count.hash(&mut h);
        p.pending.entry.hash(&mut h);
        p.pending.failed.hash(&mut h);
        p.pending.retry.hash(&mut h);
    }
    for r in rows {
        r.token.0.hash(&mut h);
        r.timestamp.hash(&mut h);
        r.pending.entry.hash(&mut h);
        r.pending.failed.hash(&mut h);
        r.pending.retry.hash(&mut h);
        r.text.is_some().hash(&mut h);
    }
    h.finish() | 1
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
    /// The user's sent direct messages, as last read from the delegate, and the text of what
    /// this app sent. The direct-message view reads it through [`Rooms::dm_cache`]; what this
    /// app sent is saved to the delegate by [`Rooms::dm_save_tick`].
    dm_cache: OutboundCache,
    /// The pacing of those saves ([`super::dm_save`]).
    dm_saver: DmSaver,
    /// The save count when the read now out began ([`OutboundCache::note_read_from`]).
    dm_read_began: Option<u64>,
    /// The threads of each room built last time, so a report that changes nothing in a room's
    /// messages does not open them again ([`Rooms::dm_snapshot`]).
    dm_memo: HashMap<VerifyingKey, DmMemo>,
    /// What the window was last sent as direct-message threads: each room's key and the key its
    /// threads were built under, whether the sent-text cache had been read, and which rooms were listed. A report sends
    /// the threads again only when this changes.
    dm_sent: Option<DmSent>,
    /// Answers to [`Action::SendDm`] not yet reported.
    dm_results: Vec<DmSendResult>,
    /// Answers to [`Action::PurgeDm`] not yet reported.
    dm_purge_results: Vec<DmPurgeResult>,
    /// Answers to [`Action::SendInviteDm`] and [`Action::AcceptInviteDm`] not yet reported.
    dm_invite_results: Vec<DmInviteResult>,
    /// What [`Rooms::prune_purged_texts`] last ran on.
    purge_pruned: Option<u64>,
    /// For each room, the winning purge-envelope version this run already re-signed against, per
    /// recipient (`OwnPurges::healed_at`): at most one re-sign per winning version
    /// ([`Rooms::own_purges_tick`]). Not kept on disk, see [`super::purge_record`].
    healed_run: HashMap<VerifyingKey, Vec<(MemberId, u64)>>,
    /// What [`Rooms::own_purges_tick`] last read for each room.
    purge_tick_seen: HashMap<VerifyingKey, u64>,
    /// Rooms whose record is to be removed (the user left): retried until it is.
    purge_forget_owed: HashSet<VerifyingKey>,
    /// Rooms where the user could not write when the tick last looked, so a re-sign waits (it is
    /// looked at again every tick; pruning is not held up by it).
    purge_heal_blocked: HashSet<VerifyingKey>,
    /// Failed storage work on the record of deletions waits until then to be tried again.
    purge_retry_at: Option<Instant>,
    /// A message was sent into a thread this copy of the archive list does not hold: the stored
    /// rooms are read again once at the next tick, and at most once in [`REREAD_EVERY`].
    reread_after_send: bool,
    reread_not_before: Option<Instant>,
    /// The pacing of the writes that carry archive changes ([`super::dm_archive`]), apart from
    /// the sent text's: a write the store keeps refusing must not delay the text, and the text
    /// must not delay it.
    archive_saver: DmSaver,
    /// River's web app on the node, which invite links open.
    web_app: String,
    /// Invitations made since the last report, by room key.
    invites: Vec<(String, Result<String, String>)>,
    /// Identity export tokens asked for, by room, waiting to be reported. Each is the user's
    /// signing key: it is held only until the next report and is never printed.
    exports: Vec<(String, Result<String, String>)>,
    /// The store went missing on the last connection: the next one registers it first.
    register_first: bool,
    /// Carrying rooms over from earlier chat delegates was started this run.
    carry_started: bool,
    /// Rooms stored under an earlier room-contract key, followed under the current one: the
    /// earlier key, by owner.
    rekeyed: HashMap<VerifyingKey, freenet_stdlib::prelude::ContractKey>,
    /// Rooms followed under their earlier key until a move to the current one succeeds.
    on_earlier: HashSet<VerifyingKey>,
    /// Rooms being moved to their current contract key now.
    seeding: HashSet<VerifyingKey>,
    /// Moves to try again: attempts so far, when, and the earlier keys to read.
    /// `None` while that move is under way.
    seed_retry: HashMap<VerifyingKey, (u32, Option<Instant>, Vec<ContractInstanceId>)>,
    /// The earlier room-contract keys each stored room's slot still owes a merge from.
    owed: HashMap<VerifyingKey, Vec<freenet_stdlib::prelude::ContractKey>>,
    /// While the store could not be set up: when the app next registers it by itself, and
    /// the wait after that one. `None` while the store is there.
    register_retry: Option<(Instant, Duration)>,
    /// Keeping the delegate's copy of each room's signing key.
    keys: super::keys::KeySweep,
    /// Imports that were stored, waiting for the room to show under the imported identity so
    /// the room's record of that member can be checked ([`Rooms::run_heals`]). With how many
    /// reports they have waited.
    heals: Vec<(super::work::ImportDone, u32)>,
    /// Rooms whose copy has taken a whole state from the network in this run, so what it holds
    /// about the room's members is not only what the store had.
    network_seen: HashSet<VerifyingKey>,
    /// River has moved on: nothing is written, and the screen says so.
    update_required: bool,
    /// The store needs registering, and waits for the first pointer check to settle.
    register_deferred: bool,
    /// The room the window shows, by key (`Room::key`), as [`Action::Viewing`] last said.
    viewing: Option<String>,
    /// The window came to the front ([`Action::Resync`]): read again the rooms not heard about
    /// lately.
    resync_wanted: bool,
    /// Each merge of what the network sent since the session last looked: the room, and
    /// whether it merged. The session reads again a room whose merge failed.
    merge_results: Vec<(ContractInstanceId, Result<(), String>)>,
}

impl Rooms {
    /// The sent-message cache, hydrated by every read of the stored rooms.
    #[cfg(test)]
    fn dm_cache(&self) -> &OutboundCache {
        &self.dm_cache
    }

    /// The direct-message threads of every room, when they differ from what the window was last
    /// sent. Read only. A room with no direct messages has no threads and costs nothing here.
    ///
    /// The threads of a room are rebuilt only when their key changes
    /// ([`LiveRoom::dm_key`]): this runs at every report, and the key is much cheaper than
    /// verifying and opening up to a thousand messages.
    fn dm_snapshot(&mut self) -> Option<DmSnapshot> {
        let Rooms {
            list,
            dm_cache,
            dm_memo,
            dm_sent,
            network_seen,
            outbox,
            complete,
            ..
        } = self;
        // The direct messages still in the outbox, by room. Their text is kept with them (it
        // outlives a session), and put in the cache so the row it becomes when the room holds
        // it keeps its text.
        // Only from a complete read of the rooms: a room that could not be read is missing from
        // the list, and its unsaved text must survive until it is back (as the outbox's does).
        if *complete {
            dm_cache.keep_sent_rooms(|owner| {
                list.iter().any(|r| r.stored.owner_vk.as_bytes() == owner)
            });
        }
        let pending: Vec<Vec<super::outbox::DmRow>> = list
            .iter()
            .map(|r| outbox.dm_rows(&r.stored.owner_vk))
            .collect();
        let purging: Vec<Vec<super::outbox::PurgeRow>> = list
            .iter()
            .map(|r| outbox.purge_rows(&r.stored.owner_vk))
            .collect();
        for (room, rows) in list.iter().zip(&pending) {
            for row in rows {
                let (owner, token) = (&room.stored.owner_vk, &row.token);
                if let Some(text) = &row.text
                    && dm_cache.plaintext(owner, row.recipient, token).is_none()
                    && dm_cache.remember_sent(
                        owner,
                        row.recipient,
                        *token,
                        row.sender,
                        row.timestamp,
                        text.as_str().to_owned(),
                    )
                {
                    crate::diag!(
                        "River: too many sent direct messages are unsaved; the oldest text was dropped"
                    );
                }
            }
        }
        let all_names: Vec<String> = list.iter().map(LiveRoom::name).collect();
        let label = |vk: &VerifyingKey| {
            list.iter()
                .find(|r| &r.stored.owner_vk == vk)
                .map(LiveRoom::name)
        };
        let with_dms: Vec<(
            &LiveRoom,
            &[super::outbox::DmRow],
            &[super::outbox::PurgeRow],
        )> = list
            .iter()
            .zip(&pending)
            .zip(&purging)
            .filter(|((r, rows), purges)| {
                !rows.is_empty()
                    || !purges.is_empty()
                    || !r.stored.room_state.direct_messages.messages.is_empty()
            })
            .map(|((r, rows), purges)| (r, rows.as_slice(), purges.as_slice()))
            .collect();
        let keys: Vec<(VerifyingKey, super::dm::DmKey, u64)> = with_dms
            .iter()
            .map(|(r, rows, purges)| {
                (
                    r.stored.owner_vk,
                    r.dm_key(dm_cache, &label, &all_names),
                    pending_digest(rows, purges),
                )
            })
            .collect();
        // The loaded rooms are in it too, by key: a room met for the first time has its messages
        // seeded as seen, so a changed list (even one of the same size) is a change here.
        // Rooms whose state has come from the network, not a stub of a room just joined.
        let room_keys: Vec<String> = list
            .iter()
            .filter(|r| network_seen.contains(&r.stored.owner_vk))
            .map(LiveRoom::key)
            .collect();
        let fingerprint = (
            keys.clone(),
            dm_cache.hydrated(),
            room_keys.clone(),
            dm_cache.archive_rev(),
        );
        if dm_sent.as_ref() == Some(&fingerprint) {
            return None;
        }
        dm_memo.retain(|vk, _| keys.iter().any(|(k, _, _)| k == vk));
        // The archive list, for the rooms listed: from the last read, with what the user did
        // since laid over it, so what the web UI wrote shows and what it removed does not.
        let archived = dm_cache
            .archived()
            .into_iter()
            .filter_map(|((owner, peer), hidden_at_ts)| {
                list.iter()
                    .find(|r| r.stored.owner_vk.as_bytes() == &owner)
                    .map(|r| crate::chat::dm_model::DmArchived {
                        room: r.key(),
                        peer_key: super::dm::peer_key(peer),
                        hidden_at_ts,
                    })
            })
            .collect();
        let mut snapshot = DmSnapshot {
            hydrated: dm_cache.hydrated(),
            room_keys,
            archived,
            ..DmSnapshot::default()
        };
        for ((room, rows, purges), (vk, key, _)) in with_dms.iter().zip(&keys) {
            snapshot
                .purges
                .extend(purges.iter().map(|p| crate::chat::dm_model::DmPurge {
                    room: room.key(),
                    peer_key: super::dm::peer_key(p.peer),
                    count: p.count,
                    pending: p.pending.clone(),
                }));
            let memo = dm_memo.entry(*vk);
            let memo = match memo {
                std::collections::hash_map::Entry::Occupied(e) if e.get().key == *key => {
                    e.into_mut()
                }
                entry => {
                    let built = room.dm_threads(dm_cache, &label);
                    let builds = match &entry {
                        std::collections::hash_map::Entry::Occupied(e) => e.get().builds + 1,
                        std::collections::hash_map::Entry::Vacant(_) => 1,
                    };
                    let value = DmMemo {
                        key: *key,
                        truncated: built.truncated,
                        threads: built.threads.into_iter().map(std::sync::Arc::new).collect(),
                        builds,
                    };
                    match entry {
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            e.insert(value);
                            e.into_mut()
                        }
                        std::collections::hash_map::Entry::Vacant(e) => e.insert(value),
                    }
                }
            };
            if memo.truncated {
                snapshot.truncated.push(room.key());
            }
            if rows.is_empty() {
                snapshot.threads.extend(memo.threads.iter().cloned());
            } else {
                snapshot
                    .threads
                    .extend(super::dm_send::with_pending(room, &memo.threads, rows));
            }
        }
        *dm_sent = Some(fingerprint);
        Some(snapshot)
    }

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
    fn replace(&mut self, mut loaded: Loaded) -> Vec<ContractInstanceId> {
        // A store that was not there says nothing about the sent messages in it. Otherwise the
        // cache takes the read, but trusts "the key was not listed" only from a list that named
        // rooms (the same distrust as the room list below: an empty list can be a failed read).
        if !loaded.store_missing {
            let authoritative = loaded.listed > 0;
            let began = self
                .dm_read_began
                .take()
                .unwrap_or_else(|| self.dm_cache.save_seq());
            self.dm_cache
                .note_read_from(std::mem::take(&mut loaded.dms), authoritative, began);
        }
        if self.loaded && (loaded.store_missing || loaded.listed == 0) && !self.list.is_empty() {
            crate::diag!("River: the stored rooms read as empty; keeping the list shown");
            // Still recorded: a store that could not be set up again is not registered again
            // on every re-read (`follow_on`).
            self.store_missing = loaded.store_missing;
            return Vec::new();
        }
        // The rooms stored now are the ones there are: a finished leave has done its work.
        self.outbox.work.rooms_read();
        self.owed = std::mem::take(&mut loaded.owed);
        let mut old: HashMap<VerifyingKey, LiveRoom> = self
            .list
            .drain(..)
            .map(|r| (r.stored.owner_vk, r))
            .collect();
        let mut new_ids = Vec::new();
        let mut by_key: HashMap<VerifyingKey, LiveRoom> = HashMap::new();
        for mut slot in loaded.slots {
            let vk = slot.owner_vk;
            // A room stored under one of its earlier contract keys is followed under the key
            // this build's room contract gives it ([`super::rekey`]).
            // While a move of it is under way (the current key had no state), it is followed
            // under the earlier key it was stored under, as before the re-key.
            match super::rekey::current_key(&slot) {
                (current, Some(earlier)) => {
                    if !self.on_earlier.contains(&vk) {
                        slot.contract_key = current;
                    }
                    self.rekeyed.insert(vk, earlier);
                }
                (_, None) => {
                    self.rekeyed.remove(&vk);
                    self.on_earlier.remove(&vk);
                }
            }
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
                        crate::diag!(
                            "River: the stored copy of a room was not merged: {}",
                            crate::diag::scrub(&e)
                        );
                        self.not_updated.insert(vk);
                    }
                    kept
                }
                Some(previous) => {
                    let mut fresh = LiveRoom::new(slot);
                    if fresh.contract_id() == previous.contract_id() {
                        // A replaced identity with its member-info check waiting needs the
                        // network's current state: ask for it again.
                        if fresh.stored.self_verifying_key() != previous.stored.self_verifying_key()
                            && self.heals.iter().any(|(d, _)| d.room == vk)
                        {
                            new_ids.push(fresh.contract_id());
                        }
                        if let Err(e) = fresh.merge_state(&previous.stored.room_state) {
                            crate::diag!(
                                "River: a room's earlier copy was not merged: {}",
                                crate::diag::scrub(&e)
                            );
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
        // What the window shows, and its coming to the front, are not the user's doing: taken at
        // once, and never kept in the outbox.
        for command in commands.try_iter() {
            match command.action {
                Action::Viewing => self.viewing = Some(command.room),
                Action::Resync => self.resync_wanted = true,
                _ => self.outbox.waiting.push(command),
            }
        }
        if !self.loaded || self.outbox.waiting.is_empty() {
            return false;
        }
        let mut changed = false;
        for command in std::mem::take(&mut self.outbox.waiting) {
            if command.action.is_list_level() {
                changed |= self.list_command(&command.room, command.action);
                continue;
            }
            if matches!(command.action, Action::SendDm { .. }) {
                // Said as an answer, not as a change to a room: the composer waits for it.
                match self.send_dm(&command) {
                    Some(()) => changed = true,
                    None => self.outbox.waiting.push(command),
                }
                continue;
            }
            if matches!(
                command.action,
                Action::SendInviteDm { .. } | Action::AcceptInviteDm { .. }
            ) {
                // Likewise: the picker and the card wait for their answers.
                match self.invite_dm(&command) {
                    Some(()) => changed = true,
                    None => self.outbox.waiting.push(command),
                }
                continue;
            }
            if matches!(
                command.action,
                Action::ArchiveDm { .. } | Action::UnarchiveDm { .. }
            ) {
                // Not answered: the thread is archived on screen as soon as this is taken.
                match self.archive_dm(&command) {
                    Some(()) => changed = true,
                    None => self.outbox.waiting.push(command),
                }
                continue;
            }
            if matches!(command.action, Action::PurgeDm { .. }) {
                // Likewise: the confirmation waits for its answer.
                match self.purge_dm(&command) {
                    Some(()) => changed = true,
                    None => self.outbox.waiting.push(command),
                }
                continue;
            }
            match self.list.iter().find(|r| r.key() == command.room) {
                Some(room) if command.action == Action::Invite => {
                    let link = super::create::invitation(room, &self.web_app).map(|(_, l)| l);
                    self.invites.push((room.key(), link));
                    changed = true;
                }
                Some(room) if command.action == Action::ExportId => {
                    self.exports
                        .push((room.key(), super::identity::export_token(room)));
                    changed = true;
                }
                Some(room) => {
                    // A direct message discarded loses the text kept for its row.
                    let discarded = match &command.action {
                        Action::Discard { entry } => self
                            .outbox
                            .dm_identity(&room.stored.owner_vk, *entry)
                            .map(|d| (room.stored.owner_vk, d)),
                        _ => None,
                    };
                    let did = self.outbox.command(room, command.action);
                    if did && let Some((owner, (recipient, token))) = discarded {
                        self.dm_cache.forget_sent(&owner, recipient, &token);
                        // Only a message whose text was saved (or is being saved) has an entry
                        // to remove, and only that entry.
                        let key = (owner.to_bytes(), recipient, token);
                        let was_saved = self.dm_cache.forget_saved(&owner, recipient, &token);
                        if was_saved || self.dm_saver.is_being_added(&key) {
                            self.dm_saver.owes_removal(key);
                        }
                    }
                    changed |= did;
                }
                // The read so far says nothing about this room (no store, nothing listed, or
                // a slot that couldn't be read): it waits for a read that does.
                None if !self.complete => self.outbox.waiting.push(command),
                // Left, in the browser, before this arrived. An invitation asked for there is
                // said to have failed, so the screen does not wait for it.
                None => {
                    crate::diag!("River: a command for a room no longer shown was dropped");
                    if command.action == Action::Invite {
                        self.invites
                            .push((command.room, Err("This room is no longer here.".into())));
                        changed = true;
                    } else if command.action == Action::ExportId {
                        self.exports
                            .push((command.room, Err("This room is no longer here.".into())));
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    /// Check, compose and queue a direct message ([`Outbox::send_dm`]) and say what became of
    /// it. `None` when the rooms have not been read well enough to say the room is gone: it
    /// waits, as any command does.
    fn send_dm(&mut self, command: &Command) -> Option<()> {
        let Action::SendDm { peer, text, ticket } = &command.action else {
            return Some(());
        };
        let result = match self.list.iter().find(|r| r.key() == command.room) {
            // River has moved on: nothing is written ([`super::generation`]).
            Some(_) if self.update_required => Err(UPDATE_REQUIRED_SEND.to_owned()),
            Some(room) => self.outbox.send_dm(room, *peer, text).map(|queued| {
                // The text is kept at once, not at the next report: the node can answer, and
                // the entry be done, before one. (The report also puts back what an outbox that
                // outlived a session still holds.)
                if let Some(sender) = queued.sender {
                    if self.dm_cache.remember_sent(
                        &room.stored.owner_vk,
                        queued.recipient,
                        queued.token,
                        sender,
                        queued.timestamp,
                        text.to_owned(),
                    ) {
                        crate::diag!("River: too many sent direct messages are unsaved; the oldest text was dropped");
                    }
                    self.dm_saver.new_work();
                    // Sending into a thread brings it back, whatever its cutoff: the user did
                    // it (a conditional unhide would be a no-op here). Only when there may be
                    // something to remove: before the archive list is read there may be. A
                    // thread another device archived since this copy was read is not seen
                    // here, and shows archived again at the next read (the web UI always asks).
                    let thread = (room.stored.owner_vk.to_bytes(), queued.recipient);
                    if !self.dm_cache.hydrated() || self.dm_cache.archived_at(&thread).is_some() {
                        self.dm_cache.archive(ArchiveOp::Unhide { thread });
                        self.archive_saver.new_work();
                    } else {
                        // This copy of the list does not hold the thread, and another device
                        // may have archived it since it was read: read it again, once, soon
                        // ([`Rooms::dm_save_tick`]), so the next send or gated sync sees it.
                        self.reread_after_send = true;
                    }
                }
            }),
            None if !self.complete => return None,
            None => Err("This room is no longer here.".to_owned()),
        };
        self.dm_results.push(DmSendResult {
            room: command.room.clone(),
            peer: super::dm::peer_key(*peer),
            ticket: *ticket,
            result,
        });
        Some(())
    }

    /// Send or accept an invitation in a direct message ([`super::dm_invite`]) and say what
    /// became of it. `None` when the rooms have not been read well enough to say the room is
    /// gone: it waits, as any command does.
    ///
    /// Sending is queued like a text message ([`Outbox::send_invite_dm`]) and its summary is
    /// kept for the sender's row like a text's. Accepting reads the invitation from the room now
    /// ([`super::dm_invite::open`]) and starts the join ([`super::work::Work::join_invitation`]),
    /// as a pasted invite code does, with the nickname the join dialog offers. Nothing here keeps
    /// the invitation: it is moved into the join, and every refusal is in fixed words.
    fn invite_dm(&mut self, command: &Command) -> Option<()> {
        let (peer, ticket) = match &command.action {
            Action::SendInviteDm { peer, ticket, .. }
            | Action::AcceptInviteDm { peer, ticket, .. } => (*peer, *ticket),
            _ => return Some(()),
        };
        let Some(carrier) = self.list.iter().find(|r| r.key() == command.room) else {
            if !self.complete {
                return None;
            }
            self.dm_invite_results.push(DmInviteResult {
                room: command.room.clone(),
                peer: super::dm::peer_key(peer),
                ticket,
                result: Err("This room is no longer here.".to_owned()),
            });
            return Some(());
        };
        let result = match &command.action {
            Action::SendInviteDm { target, note, .. } => {
                if self.update_required {
                    Err(UPDATE_REQUIRED_SEND.to_owned())
                } else if let Some(target) = self.list.iter().find(|r| &r.key() == target) {
                    self.outbox
                        .send_invite_dm(carrier, target, peer, note.as_deref())
                        .map(|(queued, summary)| {
                            // Only the summary is kept, as the web UI keeps it.
                            if let Some(sender) = queued.sender {
                                if self.dm_cache.remember_sent(
                                    &carrier.stored.owner_vk,
                                    queued.recipient,
                                    queued.token,
                                    sender,
                                    queued.timestamp,
                                    summary,
                                ) {
                                    crate::diag!("River: too many sent direct messages are unsaved; the oldest text was dropped");
                                }
                                self.dm_saver.new_work();
                            }
                        })
                } else {
                    Err("That room is no longer here.".to_owned())
                }
            }
            Action::AcceptInviteDm { token, .. } => {
                if self.update_required {
                    Err(UPDATE_REQUIRED_WORK.to_owned())
                } else {
                    let shown: HashSet<VerifyingKey> =
                        self.list.iter().map(|r| r.stored.owner_vk).collect();
                    super::dm_invite::open(carrier, peer, token).and_then(|invitation| {
                        let nickname = crate::chat::visual_fold::generate_default_nickname(
                            &invitation.invitee.member.member_vk,
                        );
                        self.outbox
                            .work
                            .join_invitation(invitation, &nickname, true, &|vk| shown.contains(vk))
                    })
                }
            }
            _ => return Some(()),
        };
        self.dm_invite_results.push(DmInviteResult {
            room: command.room.clone(),
            peer: super::dm::peer_key(peer),
            ticket,
            result,
        });
        Some(())
    }

    /// Check, build and queue a deletion ([`Outbox::purge_dm`]) and say what became of it.
    /// `None` when the rooms have not been read well enough to say the room is gone: it waits,
    /// as any command does.
    ///
    /// It is refused for a room not yet read from the network: the envelope must list what the
    /// room's current envelope lists, and a copy that only the store has read may lack a
    /// deletion made from another device, which this one would then lift.
    fn purge_dm(&mut self, command: &Command) -> Option<()> {
        let Action::PurgeDm {
            peer,
            tokens,
            ticket,
        } = &command.action
        else {
            return Some(());
        };
        let result = match self.list.iter().find(|r| r.key() == command.room) {
            // River has moved on: nothing is written ([`super::generation`]).
            Some(_) if self.update_required => Err(UPDATE_REQUIRED_PURGE.to_owned()),
            Some(room) if !self.network_seen.contains(&room.stored.owner_vk) => {
                Err(NOT_LOADED_PURGE.to_owned())
            }
            Some(room) => self.outbox.purge_dm(room, *peer, tokens),
            None if !self.complete => return None,
            None => Err("This room is no longer here.".to_owned()),
        };
        self.dm_purge_results.push(DmPurgeResult {
            room: command.room.clone(),
            peer: super::dm::peer_key(*peer),
            ticket: *ticket,
            result,
        });
        Some(())
    }

    /// Archive or unarchive a thread as the user asked ([`Action::ArchiveDm`],
    /// [`Action::UnarchiveDm`]). The change shows at once and is owed to the delegate
    /// ([`OutboundCache::archive`]). `None` when the rooms have not been read well enough to say
    /// the room is gone: it waits, as any command does.
    fn archive_dm(&mut self, command: &Command) -> Option<()> {
        let Some(room) = self.list.iter().find(|r| r.key() == command.room) else {
            if !self.complete {
                return None;
            }
            crate::diag!("River: an archive command for a room no longer shown was dropped");
            return Some(());
        };
        let owner = room.stored.owner_vk.to_bytes();
        let op = match &command.action {
            Action::ArchiveDm {
                peer,
                row_inbound_ts,
            } => {
                // The newest inbound time of the room as it is now, as well as the thread as the
                // window showed it: the window can lag a message that landed since.
                let live = room.stored.self_verifying_key().map(|vk| {
                    dm_archive::live_inbound_ts(
                        &room.stored.room_state.direct_messages,
                        MemberId::from(&vk),
                        *peer,
                    )
                });
                ArchiveOp::Hide {
                    thread: (owner, *peer),
                    hidden_at_ts: dm_archive::archive_cutoff(*row_inbound_ts, live),
                }
            }
            Action::UnarchiveDm { peer } => ArchiveOp::Unhide {
                thread: (owner, *peer),
            },
            _ => return Some(()),
        };
        self.dm_cache.archive(op);
        self.archive_saver.new_work();
        Some(())
    }

    /// Drop the archive entry of a thread whose peer has written since, the inbound-sync half of
    /// the rules ([`super::dm_archive`]): a message strictly past the stored cutoff. The rail
    /// already shows such a thread; this stops a later sweep (which leaves the thread with no
    /// messages, so nothing past any cutoff) from hiding it again. Only an entry the delegate
    /// holds and no change of the user's is owed for, and the write drops it only if it still
    /// finds a cutoff below the message. `now_secs` is the time in unix seconds.
    ///
    /// Run at every tick of the loop, with no memo of what it last saw: what it reads is a few
    /// entries against one room's messages, and a memo keyed on the room's message count would
    /// miss a revival when a pair at its cap evicts one message as another lands.
    fn archive_revival(&mut self, now_secs: u64) {
        let archived = self.dm_cache.archived();
        if archived.is_empty() {
            return;
        }
        // Each room with an entry is read once for every peer's newest inbound time, so the cost
        // is the messages of those rooms plus the entries, not entries times messages. Rooms
        // with no entry are not looked at.
        let owed: HashSet<dm_archive::ThreadKey> = self
            .dm_cache
            .archive_owed()
            .iter()
            .map(ArchiveOp::thread)
            .collect();
        let mut newest: HashMap<[u8; 32], HashMap<MemberId, u64>> = HashMap::new();
        let mut ops = Vec::new();
        for (thread, cutoff) in archived {
            if owed.contains(&thread) || self.dm_cache.revival_held(&thread) {
                continue;
            }
            let list = &self.list;
            let by_peer = newest.entry(thread.0).or_insert_with(|| {
                list.iter()
                    .find(|r| r.stored.owner_vk.as_bytes() == &thread.0)
                    .and_then(|room| {
                        let me = MemberId::from(&room.stored.self_verifying_key()?);
                        Some(dm_archive::live_inbound_by_peer(
                            &room.stored.room_state.direct_messages,
                            me,
                        ))
                    })
                    .unwrap_or_default()
            });
            let Some(&latest) = by_peer.get(&thread.1) else {
                continue;
            };
            if let Some(op) = dm_archive::revival_op(thread, cutoff, latest, now_secs) {
                ops.push(op);
            }
        }
        if !ops.is_empty() {
            for op in ops {
                self.dm_cache.archive(op);
            }
            self.archive_saver.new_work();
        }
    }

    /// Settle the sent-message writes that ended, and start the next one if one is due
    /// ([`super::dm_save`] has the rules). `writes` is the connection's write gate.
    fn dm_save_tick(&mut self, writes: bool, now: Instant) {
        // The rooms an add may be written for, as of now.
        self.outbox.work.set_dm_rooms(
            self.list
                .iter()
                .map(|r| r.stored.owner_vk.to_bytes())
                .collect(),
        );
        for end in self.outbox.work.take_dm_ends() {
            // A write carries archive changes or sent text, never both.
            // (A write whose every change was left out, for rooms that are gone, is an archive
            // write too.)
            let archive_write = !end.archived.is_empty() || !end.archive_dropped.is_empty();
            let saver = if archive_write {
                &mut self.archive_saver
            } else {
                &mut self.dm_saver
            };
            saver.ended(end.ok, now);
            let failures = saver.failures();
            if end.ok {
                self.dm_cache.note_saved(&end.added, &end.evicted);
                self.dm_cache
                    .note_archive_saved(&end.archived, &end.archive_skipped);
                self.dm_cache.note_archive_dropped(&end.archive_dropped);
                if !end.archive_skipped.is_empty() {
                    // The store held something else than this copy of the list assumed (another
                    // device got there first): read it again to show what it holds.
                    self.outbox.work.ask_reload();
                }
            } else {
                // Said once, without a word of what was written.
                let kind = end.refusal.map_or("other", |r| r.kind());
                if archive_write {
                    crate::diag!(
                        "River: saving archive changes failed ({kind}), {failures} in a row"
                    );
                } else {
                    crate::diag!(
                        "River: saving sent direct messages failed ({}), {} in a row",
                        kind,
                        failures
                    );
                }
                if end.refusal == Some(super::dm_store::Refusal::Vanished) {
                    // The key may be gone for good or only unread: read the stored rooms again,
                    // whose list says which ([`OutboundCache::note_read`]).
                    self.outbox.work.ask_reload();
                }
            }
        }
        self.prune_purged_texts();
        // Only when the read can run: the window is not used up by a request nothing will take.
        if self.reread_after_send
            && writes
            && self.loaded
            && !self.update_required
            && !self.store_missing
            && self.reread_not_before.is_none_or(|at| now >= at)
        {
            self.reread_after_send = false;
            self.reread_not_before = Some(now + REREAD_EVERY);
            self.outbox.work.ask_reload();
        }
        if !writes
            || self.update_required
            || !self.loaded
            || self.store_missing
            || self.outbox.work.has_dm()
            || !self.dm_cache.hydrated()
        {
            return;
        }
        if self.dm_saver.due(now) {
            self.start_text_write();
        }
        // Archive changes go after the text, and not at all while a write is out. They have their
        // own waits and their own count of failures.
        if !self.outbox.work.has_dm() && self.archive_saver.due(now) {
            self.start_archive_write();
        }
    }

    /// Start the write that carries archive changes, if any are owed: only those, so a store that
    /// refuses them cannot refuse a removal or a sent text with them.
    fn start_archive_write(&mut self) {
        // Taking an entry out is never refused for size, and a new one can be: so removals go
        // first and alone, and the new entries after, or a new entry that does not fit would hold
        // back an Un-archive for good.
        // A new entry for a room missing from the list waits, as a sent text does, until a
        // complete read of the rooms says the room is gone (a partial list is missing rooms for a
        // while); only then it is left out of the write as an orphan ([`super::work`]).
        let list = &self.list;
        let complete = self.complete;
        let owed: Vec<ArchiveOp> = self
            .dm_cache
            .archive_owed()
            .iter()
            .filter(|op| match op {
                ArchiveOp::Hide { thread, .. } => {
                    complete
                        || list
                            .iter()
                            .any(|r| r.stored.owner_vk.as_bytes() == &thread.0)
                }
                _ => true,
            })
            .copied()
            .collect();
        let removals: Vec<DmOp> = owed
            .iter()
            .filter(|op| !matches!(op, ArchiveOp::Hide { .. }))
            .map(|op| DmOp::Archive(*op))
            .collect();
        let ops = if removals.is_empty() {
            owed.iter().map(|op| DmOp::Archive(*op)).collect()
        } else {
            removals
        };
        self.outbox.work.dm_save(ops, self.dm_cache.listed());
    }

    /// Start the write that carries sent text and its removals, if anything is owed.
    fn start_text_write(&mut self) {
        let mut ops = Vec::new();
        let mut adding = Vec::new();
        let mut stopped = Vec::new();
        for (key, entry, resave) in self.dm_cache.unsaved() {
            // A text for a room not in the list stays for later: the list may be partial (a
            // stored room that could not be read), and nothing is written for it meanwhile.
            if !self
                .list
                .iter()
                .any(|r| r.stored.owner_vk.as_bytes() == &key.0)
            {
                continue;
            }
            if resave {
                let window = self
                    .dm_cache
                    .newer_in_pair(&key, entry.sender, entry.timestamp);
                match self.resave_wanted(&key, entry.sender) {
                    // The room has not been read from the network yet: ask again later.
                    None => continue,
                    // Gone from the room, purged, not the user's now, or older than the
                    // newest messages the pair keeps (the web UI evicted it by its own cap):
                    // not put back. The text stays in memory.
                    Some(false) => {
                        stopped.push(key);
                        continue;
                    }
                    Some(true) if window >= MAX_DM_MESSAGES_PER_PAIR => {
                        stopped.push(key);
                        continue;
                    }
                    Some(true) => {}
                }
            }
            adding.push(key);
            ops.push(DmOp::Add {
                key,
                sender: entry.sender,
                timestamp: entry.timestamp,
                text: entry.plaintext.clone(),
            });
        }
        for key in stopped {
            self.dm_cache.stop_resaving(&key);
        }
        // What is owed removing goes alone, and first: an add that is refused cannot hold it
        // back. After it has failed a few times in a row the adds go first for a turn, and it
        // goes alone also when there is nothing to add.
        let removing: Vec<EntryKey> = self.dm_saver.removals().to_vec();
        if !removing.is_empty() && (self.dm_saver.removals_first() || ops.is_empty()) {
            let ops = removing
                .iter()
                .map(|key| DmOp::Remove { key: *key })
                .collect();
            if self.outbox.work.dm_save(ops, self.dm_cache.listed()) {
                self.dm_saver.started(Vec::new(), removing);
            }
            return;
        }
        if self.outbox.work.dm_save(ops, self.dm_cache.listed()) {
            self.dm_saver.started(adding, Vec::new());
        }
    }

    /// Keep this device's record of its deletions in step with the rooms, and sign again a
    /// deletion the room lost to another device ([`super::purge_record`]).
    ///
    /// A room whose leave finished loses its record. For each room read from the network (a
    /// copy that only the store has read may lack what the room holds, and the union built from
    /// it would be wrong), with nothing of its own deletions waiting in the outbox, the record
    /// loses recipients that are no longer members, and when the room's winning envelope for
    /// the user omits a recorded token, the union is queued at the next version
    /// ([`super::dm_purge::heal`]): at most once per winning version, so two devices that both
    /// hold records trade a bounded number of envelopes. The queued envelope is an ordinary
    /// outbox entry, so it waits for the write gate ([`super::generation`]) like every write.
    fn own_purges_tick(&mut self) {
        // A leave that finished: its record goes. Storage work that failed (this, and taking
        // back the record of a refused deletion) is tried again every few seconds until it works.
        self.purge_forget_owed
            .extend(self.outbox.work.take_done_leaves());
        let now = Instant::now();
        if self.purge_retry_at.is_none_or(|at| now >= at) {
            let mut failed = false;
            let owed: Vec<VerifyingKey> = self.purge_forget_owed.iter().copied().collect();
            for vk in owed {
                self.healed_run.remove(&vk);
                self.purge_tick_seen.remove(&vk);
                match self.outbox.purge_record_mut().forget(&vk) {
                    Ok(()) => {
                        self.purge_forget_owed.remove(&vk);
                    }
                    Err(e) => {
                        failed = true;
                        crate::diag!(
                            "River: the record of a left room's deletions stays for now: {}",
                            crate::diag::scrub(&e)
                        );
                    }
                }
            }
            failed |= self.outbox.retry_takebacks();
            self.purge_retry_at = failed.then(|| now + PURGE_STORAGE_RETRY);
        }
        // Re-signed envelopes that failed are background work and are never shown.
        let dropped = self.outbox.drop_failed_heals();
        if dropped > 0 {
            crate::diag!("River: {dropped} re-signed deletion(s) were not kept and were dropped");
        }
        if self.update_required {
            return;
        }
        for i in 0..self.list.len() {
            let room = &self.list[i];
            let vk = room.stored.owner_vk;
            if !self.network_seen.contains(&vk)
                || self.outbox.purge_record().for_room(&vk).is_none()
                || self.outbox.purge_pending(&vk)
                || self.outbox.heal_in_progress(&vk)
            {
                continue;
            }
            // Only when what it depends on changed: the held envelope, who the user is, the
            // members, the room's configuration, the record. Pruning needs no permission to
            // write (it reads what the room holds); re-signing does, and a room the user cannot
            // write in is looked at again every tick, since what unblocks it (a key arriving)
            // is not one of those inputs.
            let seen = self.own_purges_fingerprint(room);
            let same = self.purge_tick_seen.get(&vk) == Some(&seen);
            if same && !self.purge_heal_blocked.contains(&vk) {
                continue;
            }
            if !same {
                self.purge_tick_seen.insert(vk, seen);
                if let Err(e) = self
                    .outbox
                    .purge_record_mut()
                    .prune(&vk, &room.stored.room_state)
                {
                    crate::diag!(
                        "River: the record of your deletions was not pruned: {}",
                        crate::diag::scrub(&e)
                    );
                }
            }
            if super::outbox::participation(room).is_err() {
                self.purge_heal_blocked.insert(vk);
                continue;
            }
            self.purge_heal_blocked.remove(&vk);
            let Some(record) = self.outbox.purge_record().for_room(&vk) else {
                continue;
            };
            let mut own = record.clone();
            self.outbox.without_abandoned(&vk, &mut own);
            own.healed_at = self.healed_run.get(&vk).cloned().unwrap_or_default();
            let Some(result) = super::dm_purge::heal(room, &mut own) else {
                continue;
            };
            match result {
                Ok(work) => {
                    // The winning version is used up only by an envelope that was queued: one
                    // that could not be built is tried again when the room changes.
                    self.healed_run.insert(vk, own.healed_at);
                    crate::diag!(
                        "River: signing your deleted messages again for a room: the room's \
                         winning deletion omits {} that this computer recorded",
                        work.tokens.len()
                    );
                    self.outbox.heal_purge(&self.list[i], work);
                }
                Err(refusal) => crate::diag!(
                    "River: your deleted messages were not signed again: {}",
                    crate::diag::scrub(refusal.text())
                ),
            }
        }
    }

    /// What [`Rooms::own_purges_tick`] reads for one room: the envelope the room holds for the
    /// user (its signature says which, and covers its version), who the user is, the members, the room's
    /// configuration signature (what pruning checks) and the record's revision.
    fn own_purges_fingerprint(&self, room: &LiveRoom) -> u64 {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.outbox.purge_record().rev().hash(&mut h);
        let state = &room.stored.room_state;
        if let Some(sk) = &room.stored.self_sk {
            let me = MemberId::from(&sk.verifying_key());
            me.0.0.hash(&mut h);
            state
                .direct_messages
                .purges
                .iter()
                .find(|p| p.recipient_id == me)
                .map(|p| p.recipient_signature.to_bytes())
                .hash(&mut h);
        }
        for m in &state.members.members {
            m.member.id().0.0.hash(&mut h);
        }
        state.configuration.signature.to_bytes().hash(&mut h);
        h.finish()
    }

    /// Drop the sent text of every message its recipient deleted, and owe the store its removal
    /// ([`OutboundCache::prune_named`]), as the web UI does when it sees the deletion
    /// (`prune_outbound_dms_for_purges`). It acts only on a token a purge envelope in a room
    /// read from the network names, for that recipient. A message merely missing from the room
    /// is never a reason: a room read early has not caught up.
    fn prune_purged_texts(&mut self) {
        // Nothing to do unless a purge envelope or the cache changed since the last pass: this
        // runs at every tick of the loop.
        let before = self.purge_fingerprint();
        if self.purge_pruned == Some(before) {
            return;
        }
        let named: HashMap<
            [u8; 32],
            HashMap<MemberId, HashSet<river_core::room_state::direct_messages::PurgeToken>>,
        > = self
            .list
            .iter()
            .filter(|r| self.network_seen.contains(&r.stored.owner_vk))
            .filter(|r| !r.stored.room_state.direct_messages.purges.is_empty())
            .map(|r| {
                let by_recipient = r
                    .stored
                    .room_state
                    .direct_messages
                    .purges
                    .iter()
                    .map(|p| (p.recipient_id, p.state.purged.iter().copied().collect()))
                    .collect();
                (r.stored.owner_vk.to_bytes(), by_recipient)
            })
            .collect();
        if !named.is_empty() {
            let pruned = self.dm_cache.prune_named(|k| {
                named
                    .get(&k.0)
                    .and_then(|m| m.get(&k.1))
                    .is_some_and(|tokens| tokens.contains(&k.2))
            });
            for (key, in_store) in pruned {
                // Also what a write now working is adding, as a discard does: the write puts
                // it in the store, so its removal follows.
                if (in_store || self.dm_saver.is_being_added(&key))
                    && !self.dm_saver.removals().contains(&key)
                {
                    self.dm_saver.owes_removal(key);
                }
            }
        }
        self.purge_pruned = Some(self.purge_fingerprint());
    }

    /// What [`Rooms::prune_purged_texts`] depends on: each room read from the network and the
    /// rank and signature of each purge envelope it holds, and the cache's epoch.
    fn purge_fingerprint(&self) -> u64 {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.dm_cache.epoch().hash(&mut h);
        for r in &self.list {
            if !self.network_seen.contains(&r.stored.owner_vk) {
                continue;
            }
            r.stored.owner_vk.as_bytes().hash(&mut h);
            for p in &r.stored.room_state.direct_messages.purges {
                p.recipient_id.0.0.hash(&mut h);
                p.state.version.hash(&mut h);
                p.recipient_signature.to_bytes().hash(&mut h);
            }
        }
        h.finish()
    }

    /// Whether an entry the delegate lost should be saved again: the message is still in its
    /// room, was sent by the user's identity there now, and its recipient has not purged it.
    /// `None` until the room has been read from the network, which is what says the room's
    /// messages are all here.
    fn resave_wanted(&self, key: &EntryKey, sender: MemberId) -> Option<bool> {
        let Some(room) = self
            .list
            .iter()
            .find(|r| r.stored.owner_vk.as_bytes() == &key.0)
        else {
            return Some(false);
        };
        if !self.network_seen.contains(&room.stored.owner_vk) {
            return None;
        }
        let me = room
            .stored
            .self_verifying_key()
            .map(|vk| MemberId::from(&vk));
        if me != Some(sender) {
            return Some(false);
        }
        let dms = &room.stored.room_state.direct_messages;
        let held = dms.messages.iter().any(|m| {
            m.purge_token() == key.2 && m.message.sender == sender && m.message.recipient == key.1
        });
        let purged = dms
            .purges
            .iter()
            .any(|p| p.recipient_id == key.1 && p.state.purged.contains(&key.2));
        Some(held && !purged)
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
                    crate::diag!("River: leaving a room no longer shown was dropped");
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
                    crate::diag!(
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
            Action::ImportId { token, replace } => {
                let shown: HashSet<VerifyingKey> =
                    self.list.iter().map(|r| r.stored.owner_vk).collect();
                self.outbox
                    .work
                    .import_identity(&token, replace, &|vk| shown.contains(vk));
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
    /// answer, no subscribe is waiting for one, and no read of the stored rooms or of a room
    /// (`other_out`) nor request of a create or join is under way (whose requests can fail the
    /// same way).
    fn failed_update_unnamed(
        &self,
        e: &ClientError,
        subs: &Subscriptions,
        other_out: bool,
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
            _ if names_nothing(e)
                && owing.len() == 1
                && !self.outbox.has_stale()
                && subs.pending.is_empty()
                && !other_out
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

    /// The room read an error that names nothing belongs to: the one read out, when exactly one
    /// is and nothing else that could fail that way is out (no subscribe waiting, no read of
    /// the stored rooms, no signing key request, no create's or join's request, no answer owed
    /// by any room, left or listed). Otherwise it could be either's, and is no one's.
    fn nameless_read(
        &self,
        e: &ClientError,
        subs: &Subscriptions,
        resyncs: &Resyncs,
        reloading: bool,
    ) -> Option<ContractInstanceId> {
        let id = resyncs.lone()?;
        let alone = names_nothing(e)
            && subs.pending.is_empty()
            && !reloading
            && !self.keys.busy()
            && !self.outbox.work.has_out()
            && !self.outbox.has_stale()
            && !self
                .list
                .iter()
                .any(|r| self.outbox.owes(&r.stored.owner_vk));
        alone.then_some(id)
    }

    /// The contract of the room the window shows, if it is listed.
    fn viewing_contract(&self) -> Option<ContractInstanceId> {
        let key = self.viewing.as_deref()?;
        self.list
            .iter()
            .find(|r| r.key() == key)
            .map(LiveRoom::contract_id)
    }

    /// Whether the room under contract `id` may not be read again now: it owes an update's
    /// answer, or a create or join holds it. Either's error could name nothing, and then could
    /// not be told from the read's.
    fn blocks_read(&self, id: &ContractInstanceId) -> bool {
        self.list
            .iter()
            .find(|r| r.contract_id() == *id)
            .is_none_or(|r| {
                self.outbox.owes(&r.stored.owner_vk) || self.outbox.work.holds(&r.stored.owner_vk)
            })
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

    /// The network answered that it has no state under `id`. When `id` is the current contract
    /// key of a room stored under an earlier one, and no move of it is under way or waiting: the
    /// room is followed under its earlier key again, and moved ([`super::rekey`]). The earlier
    /// key's id, to ask for, when that happened.
    fn seed_if_rekeyed(&mut self, id: &ContractInstanceId) -> Option<ContractInstanceId> {
        let room = self.by_contract(id)?;
        let owner = room.stored.owner_vk;
        let earlier = *self.rekeyed.get(&owner)?;
        if *id != *super::create::contract_key(&owner).id()
            || self.seeding.contains(&owner)
            || self.seed_retry.contains_key(&owner)
        {
            return None;
        }
        let candidates = self.seed_candidates(&owner, earlier);
        self.start_seed(owner, candidates);
        self.on_earlier.insert(owner);
        let room = self.list.iter_mut().find(|r| r.stored.owner_vk == owner)?;
        room.stored.contract_key = earlier;
        // Answers owed under the key left won't come under this one (as `replace` does).
        self.outbox.forget_owed(&owner);
        Some(*earlier.id())
    }

    /// The earlier keys to fold when moving `owner`'s room, stored under `earlier`: down to the
    /// deepest of that key and the keys its slot still owes a merge from (the web UI's
    /// `rekey_candidates`).
    fn seed_candidates(
        &self,
        owner: &VerifyingKey,
        earlier: freenet_stdlib::prelude::ContractKey,
    ) -> Vec<ContractInstanceId> {
        let mut owed = vec![earlier];
        owed.extend(self.owed.get(owner).into_iter().flatten().copied());
        super::rekey::candidates_down_to_owed(owner, &owed)
    }

    /// Start moving `owner`'s room from the earlier keys `candidates`.
    fn start_seed(&mut self, owner: VerifyingKey, candidates: Vec<ContractInstanceId>) {
        let Some(room) = self.list.iter().find(|r| r.stored.owner_vk == owner) else {
            return;
        };
        let (name, local) = (room.name(), room.stored.room_state.clone());
        // The keys a merge is owed from: the one it was stored under, and its slot's.
        let owed: Vec<ContractInstanceId> = self
            .rekeyed
            .get(&owner)
            .into_iter()
            .chain(self.owed.get(&owner).into_iter().flatten())
            .map(|k| *k.id())
            .collect();
        if self
            .outbox
            .work
            .seed_owing(owner, name, candidates, owed, local)
        {
            self.seeding.insert(owner);
        }
    }

    /// Take the moves that ended: a room moved is followed under its current key (its id is
    /// returned, to ask for at once), with the earlier keys that did not answer folded in later;
    /// one that did not move is tried again after a wait that doubles.
    fn seed_ends(&mut self, pace: &Pace, now: Instant) -> Vec<ContractInstanceId> {
        let mut follow = Vec::new();
        for end in self.outbox.work.take_seed_ends() {
            let owner = end.room;
            self.seeding.remove(&owner);
            let Some(earlier) = self.rekeyed.get(&owner).copied() else {
                continue;
            };
            let attempts = self.seed_retry.get(&owner).map_or(0, |(n, _, _)| *n);
            self.seed_retry.remove(&owner);
            if !self.list.iter().any(|r| r.stored.owner_vk == owner) {
                // Left meanwhile: nothing to try again.
                self.on_earlier.remove(&owner);
                continue;
            }
            if end.moved {
                self.on_earlier.remove(&owner);
                let current = super::create::contract_key(&owner);
                if let Some(room) = self.list.iter_mut().find(|r| r.stored.owner_vk == owner) {
                    room.stored.contract_key = current;
                    // Answers owed under the key left won't come under this one.
                    self.outbox.forget_owed(&owner);
                    follow.push(*current.id());
                }
                // Keys still owed are read again a bounded number of times a run (the web UI
                // gives one up after `MAX_UNFINISHED_REKEY_LOADS` loads).
                if !end.unresolved.is_empty() && attempts < MAX_FOLD_RETRIES {
                    let wait = pace
                        .resubscribe_first
                        .saturating_mul(1 << attempts.min(16))
                        .min(pace.resubscribe_max);
                    self.seed_retry.insert(
                        owner,
                        (attempts.saturating_add(1), Some(now + wait), end.unresolved),
                    );
                }
            } else {
                let wait = pace
                    .resubscribe_first
                    .saturating_mul(1 << attempts.min(16))
                    .min(pace.resubscribe_max);
                let candidates = self.seed_candidates(&owner, earlier);
                self.seed_retry.insert(
                    owner,
                    (attempts.saturating_add(1), Some(now + wait), candidates),
                );
            }
        }
        follow
    }

    /// Start the moves whose wait is over. One for a room no longer shown, or no longer
    /// stored under an earlier key, is dropped.
    fn seeds_due(&mut self, now: Instant) {
        let due: Vec<(VerifyingKey, Vec<ContractInstanceId>)> = self
            .seed_retry
            .iter()
            .filter(|(o, (_, at, _))| at.is_some_and(|at| at <= now) && !self.seeding.contains(*o))
            .map(|(o, (_, _, c))| (*o, c.clone()))
            .collect();
        for (owner, candidates) in due {
            let shown = self.list.iter().any(|r| r.stored.owner_vk == owner);
            if !shown || !self.rekeyed.contains_key(&owner) {
                self.seed_retry.remove(&owner);
                self.on_earlier.remove(&owner);
                continue;
            }
            // Under way: the attempt count is kept, and the wait is set again when it ends.
            if let Some(entry) = self.seed_retry.get_mut(&owner) {
                entry.1 = None;
            }
            self.start_seed(owner, candidates);
        }
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

    /// Record how a merge of what the network sent went. True when what shows changed: always
    /// on success, and on a failure when the room is newly named as not updated or the merge
    /// kept something (`kept`) before the step that failed.
    fn outcome(&mut self, id: &ContractInstanceId, result: Result<(), String>, kept: bool) -> bool {
        let Some(vk) = self.by_contract(id).map(|r| r.stored.owner_vk) else {
            return false;
        };
        self.merge_results.push((*id, result.clone()));
        match result {
            Ok(()) => {
                self.not_updated.remove(&vk);
                true
            }
            Err(e) => {
                crate::diag!(
                    "River: a room's update from the network was not applied: {}",
                    crate::diag::scrub(&e)
                );
                self.not_updated.insert(vk) | kept
            }
        }
    }

    fn merge_bytes(&mut self, id: &ContractInstanceId, bytes: &[u8]) -> bool {
        let Some(room) = self.list.iter_mut().find(|r| r.contract_id() == *id) else {
            return false;
        };
        let merged = match ciborium::de::from_reader::<ChatRoomStateV1, _>(bytes) {
            Ok(incoming) => room.merge_network_state(&incoming),
            Err(e) => NetworkMerge {
                changed: false,
                result: Err(format!("unreadable state: {e}")),
            },
        };
        let sent = self.outbox.reconcile(room);
        if merged.result.is_ok() {
            let owner = room.stored.owner_vk;
            self.network_seen.insert(owner);
        }
        self.outcome(id, merged.result, merged.changed) | sent
    }

    fn apply_update(&mut self, id: &ContractInstanceId, update: &UpdateData<'_>) -> bool {
        match update {
            UpdateData::State(state) => self.merge_bytes(id, state.as_ref()),
            UpdateData::StateAndDelta { state, .. } => self.merge_bytes(id, state.as_ref()),
            UpdateData::Delta(delta) => {
                let Some(room) = self.list.iter_mut().find(|r| r.contract_id() == *id) else {
                    return false;
                };
                let merged =
                    match ciborium::de::from_reader::<ChatRoomStateV1Delta, _>(delta.as_ref()) {
                        Ok(delta) => room.apply_network_delta(delta),
                        Err(e) => NetworkMerge {
                            changed: false,
                            result: Err(format!("unreadable delta: {e}")),
                        },
                    };
                let sent = self.outbox.reconcile(room);
                self.outcome(id, merged.result, merged.changed) | sent
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

    /// The web UI's member-info heal after an identity import (river#414): an imported member
    /// the room holds but holds no nickname record for would show as "Unknown" to everyone
    /// else until they next change their nickname. Their record can only be signed with their
    /// own key, so only this app can publish it: the nickname the token carries goes out as
    /// the member's own record through the ordinary nickname path ([`Action::SetNickname`]),
    /// which seals it in a private room and refuses without the secret. Not for the owner, and
    /// not for a member the room does not hold (the first message adds them, as for any
    /// pruned member). Without a nickname in the token nothing is invented.
    fn run_heals(&mut self) {
        for (done, waited) in std::mem::take(&mut self.heals) {
            let Some(i) = self
                .list
                .iter()
                .position(|r| r.stored.owner_vk == done.room)
            else {
                // Not shown: wait for it unless a complete read says it is gone.
                if !self.complete {
                    self.heals.push((done, waited));
                }
                continue;
            };
            if self.list[i].stored.self_verifying_key() != Some(done.identity) {
                // The read that shows the new identity has not come yet.
                if waited < 500 {
                    self.heals.push((done, waited + 1));
                }
                continue;
            }
            // Decided only on a copy that has taken the network's state (a stored copy may lack
            // a nickname record the room has, and a heal over it could replace a newer one), and,
            // in a private room, once the secret is here (the nickname is sealed under it).
            let ready = self.network_seen.contains(&done.room)
                && !(self.list[i].is_private() && self.list[i].current_secret().is_none());
            if !ready {
                if waited < 500 {
                    self.heals.push((done, waited + 1));
                } else if done.nickname.is_some() {
                    self.outbox.notice(
                        &self.list[i],
                        "Your nickname",
                        "Your identity was imported, but the room's nickname for it could not be \
                         checked yet, so none was published. Change your nickname to publish one."
                            .to_owned(),
                    );
                }
                continue;
            }
            let state = &self.list[i].stored.room_state;
            let id = SelfId::from(&done.identity);
            let stranded = done.identity != done.room
                && state
                    .members
                    .members
                    .iter()
                    .any(|m| m.member.member_vk == done.identity)
                && !state
                    .member_info
                    .member_info
                    .iter()
                    .any(|r| r.member_info.member_id == id);
            if let (true, Some(nickname)) = (stranded, done.nickname) {
                self.outbox
                    .command(&self.list[i], Action::SetNickname { nickname });
            }
        }
    }

    fn report(&mut self, status: LinkStatus, report: &dyn Fn(RoomsUpdate)) {
        for done in self.outbox.work.take_done_imports() {
            // The user chose this identity: the delegate's key for the room becomes it.
            self.keys.authoritative(done.room, done.identity);
            if done.fetched {
                // An add reads the room from the network and stores it as read.
                self.network_seen.insert(done.room);
            } else {
                // A replace keeps the stored state, which may be older than the network's: the
                // heal waits for a read after this ([`Rooms::replace`] asks for one).
                self.network_seen.remove(&done.room);
            }
            self.heals.push((done, 0));
        }
        if status == LinkStatus::Live {
            self.run_heals();
        }
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
        if let Some(dms) = self.dm_snapshot() {
            report(RoomsUpdate::Dms(dms));
        }
        // After the threads, so a message that was queued is in its thread when the composer
        // lets go of its text.
        for done in std::mem::take(&mut self.dm_results) {
            report(RoomsUpdate::DmSend {
                room: done.room,
                peer: done.peer,
                ticket: done.ticket,
                result: done.result,
            });
        }
        for done in std::mem::take(&mut self.dm_purge_results) {
            report(RoomsUpdate::DmPurge {
                room: done.room,
                peer: done.peer,
                ticket: done.ticket,
                result: done.result,
            });
        }
        for done in std::mem::take(&mut self.dm_invite_results) {
            report(RoomsUpdate::DmInvite {
                room: done.room,
                peer: done.peer,
                ticket: done.ticket,
                result: done.result,
            });
        }
        for (room, link) in std::mem::take(&mut self.invites) {
            report(RoomsUpdate::Invite { room, link });
        }
        for (room, token) in std::mem::take(&mut self.exports) {
            report(RoomsUpdate::Export { room, token });
        }
        // A finished task is shown once its room is in the list (so the screen can open it),
        // then it is done with.
        let shown: HashSet<VerifyingKey> = self.list.iter().map(|r| r.stored.owner_vk).collect();
        self.outbox.work.forget_done(&|vk| shown.contains(vk));
    }

    /// The connection is gone: keep showing the rooms if there are any, and say so.
    fn lost(&self, why: &str, report: &dyn Fn(RoomsUpdate)) {
        crate::diag!(
            "River: lost the connection to Freenet: {}",
            crate::diag::scrub(why)
        );
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

/// How long failed storage work on the record of deletions waits before it is tried again.
const PURGE_STORAGE_RETRY: Duration = Duration::from_secs(5);

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "own_purge_tests.rs"]
mod own_purge_tests;

#[cfg(test)]
#[path = "dm_invite_session_tests.rs"]
mod invite_dm_tests;
