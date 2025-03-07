//! Freenet API integration for chat room synchronization
//!
//! Handles WebSocket communication with Freenet network, manages room subscriptions,
//! and processes state updates.

mod constants;
pub mod connection_manager;
pub mod error;
pub mod freenet_synchronizer;
pub mod response_handler;
pub mod room_synchronizer;

pub use freenet_synchronizer::FreenetSynchronizer;
