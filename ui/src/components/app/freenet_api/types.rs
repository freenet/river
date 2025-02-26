//! Shared types and constants for Freenet API integration

use dioxus::prelude::{Global, GlobalSignal, UnboundedSender};
use freenet_stdlib::client_api::ClientRequest;

/// WebSocket URL for connecting to local Freenet node
pub const WEBSOCKET_URL: &str = "ws://localhost:50509/v1/contract/command?encodingProtocol=native";

/// Represents the current synchronization status with the Freenet network
#[derive(Clone, Debug)]
pub enum SyncStatus {
    /// Attempting to establish connection
    Connecting,
    /// Successfully connected to Freenet
    Connected,
    /// Actively synchronizing room state
    Syncing,
    /// Error state with associated message
    Error(String),
}

/// Global signal tracking the current sync status
pub static SYNC_STATUS: GlobalSignal<SyncStatus> = Global::new(|| SyncStatus::Connecting);

/// Sender handle for making requests to the Freenet API
#[derive(Clone)]
pub struct FreenetApiSender {
    /// Channel sender for client requests
    pub request_sender: UnboundedSender<ClientRequest<'static>>,
}
