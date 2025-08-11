//! Handles pending room invitations and join requests
//!
//! This module manages the state of room invitations that are in the process
//! of being accepted or retrieved.

use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::room_state::member::AuthorizedMember;
use std::collections::HashMap;

/// Collection of pending room join requests
#[derive(Clone, Debug, Default)]
pub struct PendingInvites {
    /// Map of room owner keys to pending join information
    pub map: HashMap<VerifyingKey, PendingRoomJoin>, // TODO: Make this private and use methods to access
}

impl PendingInvites {
    /// Creates a new instance of `PendingInvites`
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
    /*
        /// Adds a new pending room join request
        pub fn add(&mut self, owner_key: VerifyingKey, join_info: PendingRoomJoin) {
            self.map.insert(owner_key, join_info);
        }

        /// Removes a pending room join request
        pub fn remove(&mut self, owner_key: &VerifyingKey) {
            self.map.remove(owner_key);
        }
    */
}

/// Information about a pending room join
#[derive(Clone, Debug)]
pub struct PendingRoomJoin {
    /// The authorized member data for the join
    pub authorized_member: AuthorizedMember,
    /// The signing key for the invited member
    pub invitee_signing_key: SigningKey,
    /// User's preferred nickname for this room
    pub preferred_nickname: String,
    /// Current status of the join request
    pub status: PendingRoomStatus,
}

/// Status of a pending room join request
#[derive(Clone, Debug, PartialEq)]
pub enum PendingRoomStatus {
    /// Ready to subscribe to room data
    PendingSubscription,
    /// Subscription request sent, waiting for response
    Subscribing,
    /// Successfully subscribed and retrieved room data
    Subscribed,
    /// Error occurred during subscription or retrieval
    Error(String),
}
