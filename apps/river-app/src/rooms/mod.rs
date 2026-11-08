//! The user's real rooms, read from the local Freenet node.
//!
//! The rooms live in River's chat delegate, stored there by the web UI under the River web
//! app's identity. [`Link`] gets that identity through a user-approved app grant ([`grant`]),
//! opens the node's client API with it ([`socket`]), reads the stored rooms ([`delegate`],
//! `river_core::stored_rooms`), shows them, then subscribes to each room contract and keeps
//! them current ([`room`]).
//!
//! It only reads. It never PUTs (not even the web UI's seed PUT for a room the network
//! doesn't have), never sends an UPDATE, and never writes delegate storage.

pub mod delegate;
pub mod grant;
pub mod room;
#[cfg(test)]
mod room_tests;
mod session;
pub mod socket;
pub mod token;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use session::{Access, LinkStatus, RoomsUpdate};

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
}

impl Link {
    /// A link to the node at [`NODE_AUTHORITY`] that hands each update to `report` (from a
    /// worker thread).
    pub fn new(
        config_dir: Option<PathBuf>,
        report: impl Fn(RoomsUpdate) + Send + Sync + 'static,
    ) -> Self {
        Link {
            inner: Arc::new(Inner {
                report: Box::new(report),
                config_dir,
                authority: NODE_AUTHORITY.to_owned(),
                running: AtomicBool::new(false),
            }),
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
                session::run(&inner.authority, inner.config_dir.as_deref(), &report)
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
