//! The user's real rooms, read from the local Freenet node.
//!
//! The rooms live in River's chat delegate, stored there by the web UI under the River web
//! app's identity. [`Link`] gets that identity through a user-approved app grant ([`grant`]),
//! opens the node's client API with it ([`socket`]), reads the stored rooms ([`delegate`],
//! `river_core::stored_rooms`), shows them, then subscribes to each room contract and keeps
//! them current ([`room`]).
//!
//! It talks to whatever answers on the node's loopback port. A local program that took that
//! port before Freenet could pose as the node: it would receive the grant token, and could show
//! made-up rooms. The web UI places the same trust in the port, and Freenet does not treat
//! other programs of the same user as a boundary either (freenet-core `app_grants.rs`, "What
//! this does NOT protect against").
//!
//! It sends what the user writes in a room they are a member of: messages, replies, edits and
//! deletions of their own messages, and reactions, each as an UPDATE of the room contract
//! ([`outbox`]). It never PUTs (not even the web UI's seed PUT for a room the network doesn't
//! have), never creates, joins or invites, and never writes delegate storage: the web UI owns
//! that, and two writers to it are what would lose data.

pub mod delegate;
pub mod grant;
pub mod outbox;
pub mod room;
#[cfg(test)]
mod room_tests;
mod session;
pub mod socket;
pub mod token;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

pub use outbox::{Action, Command};
pub use session::{Access, Issues, LinkStatus, RoomsUpdate};

/// The node's client API on this computer.
pub const NODE_AUTHORITY: &str = "127.0.0.1:7509";

/// Starts and owns the background work that reads the rooms. Cloning shares it.
#[derive(Clone)]
pub struct Link {
    inner: Arc<Inner>,
}

struct Inner {
    report: Box<dyn Fn(RoomsUpdate) + Send + Sync>,
    /// Where the grant token is kept. `None` keeps it in memory only, for this run.
    config_dir: Option<PathBuf>,
    authority: String,
    running: AtomicBool,
    /// What the user did, for the background work. The receiving end is held by the one
    /// session running at a time.
    commands: Mutex<Sender<Command>>,
    inbox: Mutex<Receiver<Command>>,
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
        let (tx, rx) = channel();
        Link {
            inner: Arc::new(Inner {
                report: Box::new(report),
                config_dir,
                authority: authority.to_owned(),
                running: AtomicBool::new(false),
                commands: Mutex::new(tx),
                inbox: Mutex::new(rx),
            }),
        }
    }

    /// Hand something the user did to the background work. It shows in the next update.
    pub fn send(&self, command: Command) {
        if let Ok(tx) = self.inner.commands.lock() {
            // The receiver lives as long as the link, so this cannot fail.
            let _ = tx.send(command);
        }
    }

    /// Start reading the rooms, unless that is already under way. The work stops by itself
    /// only on a state that needs the user (a denial, a node too old for grants); calling
    /// this again then starts over.
    pub fn start(&self) {
        if self.inner.running.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            let report = |u: RoomsUpdate| (inner.report)(u);
            let end = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Only one session runs at a time (`running`), so this is never contended; a
                // poisoned lock only means an earlier session panicked, which is reported.
                let inbox = inner
                    .inbox
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                session::run(
                    &inner.authority,
                    inner.config_dir.as_deref(),
                    &report,
                    &session::Pace::default(),
                    &|| false,
                    &inbox,
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
            report(RoomsUpdate::Access(end));
        });
    }
}
