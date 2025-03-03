//! Freenet API integration for chat room synchronization
//!
//! Handles WebSocket communication with Freenet network, manages room subscriptions,
//! and processes state updates.

mod sync_status;
mod constants;
pub mod freenet_synchronizer;

// Re-export the main types for use elsewhere
pub use sync_status::SyncStatus;
pub use freenet_synchronizer::FreenetSynchronizer;
pub use freenet_synchronizer::FreenetSynchronizerExt;
