//! Freenet API integration for chat room synchronization
//!
//! Handles WebSocket communication with Freenet network, manages room subscriptions,
//! and processes state updates.

// Re-export the main API components
pub use self::freenet::types::{FreenetApiSender, SyncStatus, SYNC_STATUS};
pub use self::freenet::synchronizer::FreenetApiSynchronizer;

// Use the existing modules in the freenet directory
pub mod freenet;

// This file is now just a re-export module
