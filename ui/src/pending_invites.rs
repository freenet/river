use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::AuthorizedMember;
use std::collections::HashMap;

#[derive(Clone)]
pub struct PendingRoomJoin {
    pub authorized_member: AuthorizedMember,
    pub preferred_nickname: String,
    pub status: PendingRoomStatus,
}

#[derive(Clone, PartialEq)]
pub enum PendingRoomStatus {
    Retrieving,
    Retrieved,
    Error(String),
}

#[derive(Clone, Default)]
pub struct PendingInvites {
    pub map: HashMap<VerifyingKey, PendingRoomJoin>,
}

// Global signal for pending invites
pub static PENDING_INVITES: GlobalSignal<PendingInvites> = Global::new(PendingInvites::default);
