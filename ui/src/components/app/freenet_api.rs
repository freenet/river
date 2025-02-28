//! Freenet API integration for chat room synchronization
//!
//! Handles WebSocket communication with Freenet network, manages room subscriptions,
//! and processes state updates.

mod sync_status;
mod constants;
mod freenet_api_sender;
mod freenet_api_synchronizer;

// Re-export the main types if desired, so you can use them easily elsewhere.
pub use sync_status::SyncStatus;
pub use freenet_api_synchronizer::FreenetApiSynchronizer;
