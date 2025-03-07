//! Freenet API integration for chat room synchronization
//!
//! Handles WebSocket communication with Freenet network, manages room subscriptions,
//! and processes state updates.

mod constants;
pub mod error;
pub mod freenet_synchronizer;

pub use freenet_synchronizer::FreenetSynchronizer;
