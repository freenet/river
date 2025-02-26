//! Freenet API integration for chat room synchronization
//!
//! Handles WebSocket communication with Freenet network, manages room subscriptions,
//! and processes state updates.

// Re-export the main API components
pub use crate::components::app::freenet::types::{FreenetApiSender, SyncStatus, SYNC_STATUS};
pub use crate::components::app::freenet::synchronizer::FreenetApiSynchronizer;

// This file is now just a re-export module
