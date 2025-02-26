//! Freenet API integration for chat room synchronization
//!
//! Handles WebSocket communication with Freenet network, manages room subscriptions,
//! and processes state updates.

// Re-export the main API components
pub use self::types::{FreenetApiSender, SyncStatus, SYNC_STATUS};
pub use self::synchronizer::FreenetApiSynchronizer;

// Module declarations
pub mod types;
pub mod connection;
pub mod processor;
pub mod subscription;
pub mod synchronizer;
