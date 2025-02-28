use dioxus::prelude::Global;
use dioxus::prelude::GlobalSignal;

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
