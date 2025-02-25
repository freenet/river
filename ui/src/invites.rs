//! Handles pending room invitations and join requests
//!
//! This module manages the state of room invitations that are in the process
//! of being accepted or retrieved.

use dioxus::prelude::{Global, GlobalSignal};
use ed25519_dalek::VerifyingKey;
use river_common::room_state::member::AuthorizedMember;
use std::collections::HashMap;

/// Global signal for tracking pending room invitations
pub static PENDING_INVITES: GlobalSignal<PendingInvites> = Global::new(|| PendingInvites {
    map: HashMap::new(),
});

/// Collection of pending room join requests
#[derive(Clone, Debug, Default)]
pub struct PendingInvites {
    /// Map of room owner keys to pending join information
    pub map: HashMap<VerifyingKey, PendingRoomJoin>,
}

/// Information about a pending room join
#[derive(Clone, Debug)]
pub struct PendingRoomJoin {
    /// The authorized member data for the join
    pub authorized_member: AuthorizedMember,
    /// User's preferred nickname for this room
    pub preferred_nickname: String,
    /// Current status of the join request
    pub status: PendingRoomStatus,
}

/// Status of a pending room join request
#[derive(Clone, Debug)]
pub enum PendingRoomStatus {
    /// Currently retrieving room data
    Retrieving,
    /// Successfully retrieved room data
    Retrieved,
    /// Error occurred during retrieval
    Error(String),
}
