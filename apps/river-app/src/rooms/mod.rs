//! The user's real rooms, read from the local Freenet node.
//!
//! The rooms live in River's chat delegate, under the River web app's identity, where the web
//! UI keeps them too. [`Link`] gets that identity through a user-approved app grant
//! ([`grant`]), opens the node's client API with it ([`socket`]), reads the stored rooms
//! ([`delegate`], `river_core::stored_rooms`), shows them, then subscribes to each room
//! contract and keeps them current ([`room`]). If the node has no chat delegate (River never
//! ran in a browser on it), the app registers it on that connection, which sets up River's
//! storage. It registers only on a grant connection: the node records the identity a
//! delegate was first registered under, for good.
//!
//! It talks to whatever answers at the node's address, `127.0.0.1:7509` unless the person chose
//! another ([`crate::address`]); the trust below is the same wherever that is, and for a node on
//! another machine reached through a tunnel the tunnel's local end is that address.
//! It talks to whatever answers on the node's loopback port. A local program that took that
//! port before Freenet could pose as the node: it would receive the grant token, and could show
//! made-up rooms. The web UI places the same trust in the port, and Freenet does not treat
//! other programs of the same user as a boundary either (freenet-core `app_grants.rs`, "What
//! this does NOT protect against").
//!
//! It sends what the user writes in a room they are a member of: messages, replies, edits and
//! deletions of their own messages, reactions, a change of their own nickname and (for a room's
//! owner) changes to the room's details, each as an UPDATE of the room contract ([`outbox`]).
//! It makes invitations, creates rooms and joins them from invitations
//! ([`create`], [`work`]), and leaves them ([`leave`]). Those are the only times it writes the
//! chat delegate's storage, which the web UI writes too, at the same time: it writes only with
//! the web UI's generation compare-and-swap, only adds a room where none is stored (or, to
//! leave one, replaces it with the web UI's tombstone), and in `rooms_meta` only changes the
//! open room, the user's per-room notification modes and, when a room is left, the entries
//! naming it ([`work`] has the details, [`modes`] the modes). It also imports an identity the
//! user pasted ([`identity`]): it adds the room where none is stored (or a tombstone is), or, only
//! when the user said replace, swaps the identity fields of the stored room in place and keeps
//! everything else in the slot. The one other thing it writes is each room's signing
//! key, in the delegate's own namespace for keys, only where the delegate holds none, or holds
//! another after the user imported that room's identity ([`keys`]).
//! It PUTs a room it has just created, under that room's own contract, and, across a room-contract
//! re-key, a room whose current key answered that it has no state: the room's state carried from
//! its earlier keys, or its own copy when none holds it, as the web UI seeds one ([`rekey`]).
//! Before River re-keys, it also carries the user's rooms over from earlier chat delegates
//! ([`carry`]).
//!
//! It writes none of that once River has moved to a newer chat delegate or room contract than
//! the one it carries ([`generation`]): it would be writing where the web UI no longer reads.
//! It then only shows the rooms and asks the user to update it.
//!
//! It also reads the plaintext of the direct messages the user sent (the delegate's
//! `outbound_dms` key, [`dm_cache`]), and builds direct-message threads from it and the rooms
//! ([`dm`]). It sends a direct message as an UPDATE of the room contract like any other change
//! ([`dm_send`], [`outbox`]), and saves the text it sent to `outbound_dms` ([`dm_save`]), the one
//! way it writes that key: a read-merge-compare-and-swap that adds or removes its own entries and
//! keeps everything else in the value as it found it ([`dm_store`]). It deletes the messages
//! someone sent the user by signing a purge envelope, the same update ([`dm_purge`]). It archives
//! a thread by editing `hidden_threads` of the same value, in the same write ([`dm_archive`]).

pub mod bans;
pub mod carry;
pub mod create;
pub mod delegate;
pub mod deputies;
pub mod dm;
pub mod dm_archive;
pub mod dm_cache;
pub mod dm_invite;
pub mod dm_purge;
pub mod dm_save;
pub mod dm_send;
pub mod dm_store;
#[cfg(test)]
mod dm_tests;
pub mod generation;
pub mod grant;
pub mod identity;
pub mod keys;
pub mod leave;
#[cfg(test)]
mod link_tests;
pub mod modes;
pub mod outbox;
pub mod purge_record;
pub mod rekey;
mod resync;
pub mod room;
#[cfg(test)]
mod room_tests;
mod session;
pub mod socket;
pub mod token;
pub mod work;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

pub use identity::IdentityToken;
pub use outbox::{Action, Command, InviteCode};
pub use session::{Access, Issues, LinkStatus, RoomsUpdate};

/// The node's client API on this computer, which River uses unless told another
/// ([`crate::address`]).
pub const NODE_AUTHORITY: &str = crate::address::DEFAULT_AUTHORITY;

/// Starts and owns the background work that reads the rooms. Cloning shares it.
#[derive(Clone)]
pub struct Link {
    inner: Arc<Inner>,
}

struct Inner {
    /// Gets each update with the node generation it was read under ([`Link::generation`]).
    report: Box<dyn Fn(RoomsUpdate, u64) + Send + Sync>,
    /// Where the grant token is kept. `None` keeps it in memory only, for this run.
    config_dir: Option<PathBuf>,
    /// The node's `host:port`, read when a session starts. Changed only by [`Link::set_node`].
    authority: Mutex<String>,
    /// Whether the person chose this node (see [`token::node_dir`]).
    explicit: AtomicBool,
    /// Bumped by every [`Link::set_node`]: a session of an older number stops.
    generation: AtomicU64,
    /// The generation the running session started under.
    running_generation: AtomicU64,
    /// A node change is waiting for the running session to stop, then a new one starts.
    restart: AtomicBool,
    /// The next session starts without what the user sent to the previous node.
    drop_pending: AtomicBool,
    running: AtomicBool,
    /// What the user did, for the background work. The receiving end is held by the one
    /// session running at a time.
    commands: Mutex<Sender<Command>>,
    inbox: Mutex<Receiver<Command>>,
    /// What the user sent that the node has not taken, kept while no session runs.
    outbox: Mutex<outbox::Outbox>,
    /// Whether River has moved on, for every session of this link. A node of its own has its
    /// own answer, so [`Link::set_node`] replaces it, unless it was given by
    /// [`Link::with_watch`].
    watch: Mutex<Arc<generation::Watch>>,
    replace_watch: Mutex<bool>,
    /// The room the window shows, as last told by [`Link::viewing`]: told again to each new
    /// session, which reads it again when it has heard nothing about it for a while.
    viewing: Mutex<Option<String>>,
}

impl Link {
    /// A link to the node at [`NODE_AUTHORITY`] that hands each update to `report` (from a
    /// worker thread).
    pub fn new(
        config_dir: Option<PathBuf>,
        report: impl Fn(RoomsUpdate) + Send + Sync + 'static,
    ) -> Self {
        Link::with_authority(NODE_AUTHORITY, config_dir, report)
    }

    /// A link to a node whose client API is at `authority` (`127.0.0.1:<port>`), such as a
    /// test node on another port. The node honours a grant only over loopback.
    pub fn with_authority(
        authority: &str,
        config_dir: Option<PathBuf>,
        report: impl Fn(RoomsUpdate) + Send + Sync + 'static,
    ) -> Self {
        Link::for_node(authority, false, config_dir, move |u, _| report(u))
    }

    /// A link to the node at `authority`, which the person chose when `explicit`. `report`
    /// also gets the node generation each update was read under, so the app can drop what a
    /// node it has left still delivers ([`Link::generation`]).
    pub fn for_node(
        authority: &str,
        explicit: bool,
        config_dir: Option<PathBuf>,
        report: impl Fn(RoomsUpdate, u64) + Send + Sync + 'static,
    ) -> Self {
        let watch = generation::Watch::shipped(config_dir.as_deref());
        let link = Link::build(authority, config_dir, watch, Box::new(report));
        link.inner.explicit.store(explicit, Ordering::Release);
        *link
            .inner
            .replace_watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        link
    }

    /// A link that watches for a newer River with `watch` rather than River's own pointer
    /// records: for end-to-end tests against a node holding test records. The app itself uses
    /// [`Link::for_node`].
    pub fn with_watch(
        authority: &str,
        config_dir: Option<PathBuf>,
        watch: generation::Watch,
        report: impl Fn(RoomsUpdate) + Send + Sync + 'static,
    ) -> Self {
        Link::build(
            authority,
            config_dir,
            watch,
            Box::new(move |u, _| report(u)),
        )
    }

    fn build(
        authority: &str,
        config_dir: Option<PathBuf>,
        watch: generation::Watch,
        report: Box<dyn Fn(RoomsUpdate, u64) + Send + Sync>,
    ) -> Self {
        let (tx, rx) = channel();
        Link {
            inner: Arc::new(Inner {
                report,
                config_dir,
                authority: Mutex::new(authority.to_owned()),
                explicit: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                running_generation: AtomicU64::new(0),
                restart: AtomicBool::new(false),
                drop_pending: AtomicBool::new(false),
                running: AtomicBool::new(false),
                commands: Mutex::new(tx),
                inbox: Mutex::new(rx),
                outbox: Mutex::new(outbox::Outbox::default()),
                watch: Mutex::new(Arc::new(watch)),
                replace_watch: Mutex::new(false),
                viewing: Mutex::new(None),
            }),
        }
    }

    /// The number of the node in use: it grows with every [`set_node`](Link::set_node), and
    /// every update is delivered with the number it was read under.
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    /// Use the node at `authority` from now on, which the person chose when `explicit`. What
    /// the link read from the previous node (its rooms, what the user sent to it, what it
    /// learnt about a newer River) belongs to that node and is dropped. Each node keeps its own
    /// saved permission ([`token::node_dir`]), so the previous node's is never presented to
    /// this one. A running session stops, and the next [`start`](Link::start) reads from the
    /// new node. Returns the new [`generation`](Link::generation).
    pub fn set_node(&self, authority: &str, explicit: bool) -> u64 {
        let inner = &self.inner;
        *inner
            .authority
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = authority.to_owned();
        inner.explicit.store(explicit, Ordering::Release);
        if *inner
            .replace_watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            *inner
                .watch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Arc::new(generation::Watch::shipped(inner.config_dir.as_deref()));
        }
        inner.drop_pending.store(true, Ordering::Release);
        // The room shown was the previous node's; the new chat tells it again when one opens.
        *inner
            .viewing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        inner.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Hand something the user did to the background work. It shows in the next update.
    pub fn send(&self, command: Command) {
        if let Ok(tx) = self.inner.commands.lock() {
            // The receiver lives as long as the link, so this cannot fail.
            let _ = tx.send(command);
        }
    }

    /// The window now shows the room keyed `room` (`Room::key`). The background work reads
    /// the room shown again from the node when it has heard nothing about it for a while
    /// (freenet/river#971), as the web UI's watchdog does.
    pub fn viewing(&self, room: &str) {
        *self
            .inner
            .viewing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(room.to_owned());
        self.send(Command {
            room: room.to_owned(),
            action: Action::Viewing,
        });
    }

    /// What was handed to the background work and not yet taken (tests).
    #[cfg(test)]
    pub(crate) fn take_sent(&self) -> Vec<Command> {
        self.inner
            .inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_iter()
            .collect()
    }

    /// The window came to the front: read again every room the node has said nothing about
    /// lately, as the web UI does when its page becomes visible.
    pub fn resync(&self) {
        self.send(Command {
            room: String::new(),
            action: Action::Resync,
        });
    }

    /// Start reading the rooms, unless that is already under way. The work stops by itself
    /// only on a state that needs the user (a denial, a node too old for grants); calling
    /// this again then starts over.
    pub fn start(&self) {
        let generation = loop {
            let generation = self.inner.generation.load(Ordering::Acquire);
            if !self.inner.running.swap(true, Ordering::AcqRel) {
                break generation;
            }
            // A session for the node before a change is stopping: start for the new one as
            // it ends. (A session that is merely waiting for the user is left alone.)
            if self.inner.running_generation.load(Ordering::Acquire) == generation {
                return;
            }
            self.inner.restart.store(true, Ordering::Release);
            if self.inner.running.load(Ordering::Acquire) {
                return;
            }
            // It ended between the two looks: start now.
        };
        self.inner
            .running_generation
            .store(generation, Ordering::Release);
        let me = self.clone();
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            // Nothing a stopped session still has to say is about the node now in use.
            let report = |u: RoomsUpdate| {
                if inner.generation.load(Ordering::Acquire) == generation {
                    (inner.report)(u, generation)
                }
            };
            let end = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Only one session runs at a time (`running`), so this is never contended; a
                // poisoned lock only means an earlier session panicked, which is reported.
                let inbox = inner
                    .inbox
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut outbox = inner
                    .outbox
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if inner.drop_pending.swap(false, Ordering::AcqRel) {
                    *outbox = outbox::Outbox::default();
                    while inbox.try_recv().is_ok() {}
                }
                // Each session starts knowing nothing of the window: tell it the room shown.
                let viewing = inner
                    .viewing
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let Some(room) = viewing {
                    me.send(Command {
                        room,
                        action: Action::Viewing,
                    });
                }
                let authority = inner
                    .authority
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let watch = inner
                    .watch
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                // The permission saved for this node, in the directory its grants live in.
                let token_dir = inner.config_dir.as_deref().map(|d| {
                    token::node_dir(d, &authority, inner.explicit.load(Ordering::Acquire))
                });
                session::run(
                    &authority,
                    token_dir.as_deref(),
                    &report,
                    &session::Pace::default(),
                    &|| inner.generation.load(Ordering::Acquire) != generation,
                    &inbox,
                    &mut outbox,
                    &watch,
                )
            }))
            .unwrap_or_else(|_| {
                Access::Refused(
                    "Something unexpected went wrong while reading your rooms. Try again.".into(),
                )
            });
            // Free the link BEFORE reporting where it stopped, so the button that state shows
            // works the moment it appears.
            inner.running.store(false, Ordering::Release);
            if inner.generation.load(Ordering::Acquire) != generation {
                // Stopped for a node change: what it ended with is about the old node.
                if inner.restart.swap(false, Ordering::AcqRel) {
                    me.start();
                }
                return;
            }
            report(RoomsUpdate::Access(end));
        });
    }
}
