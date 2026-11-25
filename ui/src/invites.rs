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
    pub(crate) fn any_in_progress(&self) -> bool {
        self.map.values().any(|join| join.status.is_in_progress())
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
#[derive(Clone)]
pub struct PendingRoomJoin {
    /// The authorized member data for the join
    pub authorized_member: AuthorizedMember,
    /// The signing key for the invited member
    pub invitee_signing_key: SigningKey,
    /// User's preferred nickname for this room
    pub preferred_nickname: String,
    /// Current status of the join request
    pub status: PendingRoomStatus,
    /// Timestamp (ms since epoch) when the status moved to `Subscribing`,
    /// used to detect stuck invitations that need retry.
    pub subscribing_since: Option<f64>,
    /// Number of times this invitation GET has been retried after timeout.
    /// On retry_count >= 1, falls back to requesting contract code from the
    /// network in case the local copy is stale.
    pub retry_count: u32,
    /// Room secrets carried in the invitation, copied here from
    /// `Invitation::room_secrets` at accept time so the pending-invite
    /// GET-response handler can seed them into `RoomData`. Empty for
    /// public rooms and pre-feature invitations.
    pub room_secrets: Vec<(u32, [u8; 32])>,
    /// How many GETs this invitation has sent on its own, and since when.
    pub get_budget: InviteGetBudget,
}

/// Bounds the GETs an invitation sends without the user asking: at most
/// `MAX_AUTO_INVITE_GETS`, all within `INVITE_GIVE_UP_MS` of the first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InviteGetBudget {
    /// When the first GET went out (ms since epoch). Retries do not move it.
    pub first_get_at: Option<f64>,
    /// GETs sent since the budget last started, the first one included.
    pub gets_sent: u32,
    /// Increases on every GET and never goes back, so a retry timer armed for
    /// an older GET can tell it is stale.
    pub generation: u64,
}

impl InviteGetBudget {
    /// Record a GET going out; returns its generation.
    pub fn record_get_sent(&mut self, now: f64) -> u64 {
        self.first_get_at.get_or_insert(now);
        self.gets_sent += 1;
        self.generation += 1;
        self.generation
    }

    /// Whether one more automatic GET, sent `delay_ms` from `now`, fits.
    pub fn may_retry(&self, now: f64, delay_ms: u64) -> bool {
        use crate::components::app::freenet_api::constants::{
            INVITE_GIVE_UP_MS, MAX_AUTO_INVITE_GETS,
        };
        self.gets_sent < MAX_AUTO_INVITE_GETS
            && self
                .first_get_at
                .is_none_or(|first| now + delay_ms as f64 - first <= INVITE_GIVE_UP_MS as f64)
    }

    /// Start a fresh budget for a user-requested retry. The generation keeps
    /// counting up so older timers stay stale.
    pub fn restart(&mut self) {
        self.first_get_at = None;
        self.gets_sent = 0;
    }
}

/// Shown (after "Failed to retrieve room: ") when the invitation's GETs keep
/// coming back NotFound.
pub const INVITE_NOT_FOUND_ERROR: &str =
    "the room was not found on the network yet. Check the invitation, or try again.";

/// Shown when the invitation's GETs keep going unanswered.
pub const INVITE_NO_ANSWER_ERROR: &str =
    "the network did not answer. Check your connection, or try again.";

/// What an invitation does after its GET came back NotFound.
#[derive(Debug, PartialEq)]
pub enum InviteNotFoundStep {
    /// GET again after `delay_ms`, if `generation` is still the latest GET.
    RetryAfter { delay_ms: u64, generation: u64 },
    /// Out of budget: the invitation now shows an error.
    GaveUp,
    /// The invitation was not waiting on a GET; nothing to do.
    NotWaiting,
}

impl PendingRoomJoin {
    /// A GET for this invitation came back NotFound.
    pub fn on_not_found(&mut self, now: f64, delay_ms: u64) -> InviteNotFoundStep {
        if !matches!(self.status, PendingRoomStatus::Subscribing) {
            return InviteNotFoundStep::NotWaiting;
        }
        if self.get_budget.may_retry(now, delay_ms) {
            InviteNotFoundStep::RetryAfter {
                delay_ms,
                generation: self.get_budget.generation,
            }
        } else {
            self.status = PendingRoomStatus::Error(INVITE_NOT_FOUND_ERROR.to_string());
            InviteNotFoundStep::GaveUp
        }
    }

    /// A NotFound retry timer armed for `generation` fired. Re-queues the
    /// invitation only if it is still waiting on that same GET; returns
    /// whether it did. A newer GET or a finished invitation makes it a no-op.
    pub fn fire_not_found_retry(&mut self, generation: u64) -> bool {
        if matches!(self.status, PendingRoomStatus::Subscribing)
            && self.get_budget.generation == generation
        {
            self.status = PendingRoomStatus::PendingSubscription;
            self.subscribing_since = None;
            self.retry_count += 1;
            true
        } else {
            false
        }
    }

    /// The invitation has waited `INVITATION_TIMEOUT_MS` without an answer.
    /// Re-queues it while the budget allows (returns `true`), otherwise shows
    /// an error (returns `false`).
    pub fn on_stuck(&mut self, now: f64) -> bool {
        if self.get_budget.may_retry(now, 0) {
            self.status = PendingRoomStatus::PendingSubscription;
            self.subscribing_since = None;
            self.retry_count += 1;
            true
        } else {
            self.status = PendingRoomStatus::Error(INVITE_NO_ANSWER_ERROR.to_string());
            false
        }
    }
}

/// Hand-written `Debug` that REDACTS `room_secrets` — the derived `Debug`
/// for `[u8; 32]` would print every room-secret byte if a `PendingRoomJoin`
/// were ever `{:?}`-logged. `SigningKey`'s own `Debug` is already
/// non-exhaustive, so it is safe to delegate to.
impl std::fmt::Debug for PendingRoomJoin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingRoomJoin")
            .field("authorized_member", &self.authorized_member)
            .field("invitee_signing_key", &self.invitee_signing_key)
            .field("preferred_nickname", &self.preferred_nickname)
            .field("status", &self.status)
            .field("subscribing_since", &self.subscribing_since)
            .field("retry_count", &self.retry_count)
            .field(
                "room_secrets",
                &format_args!("<{} room secret(s) redacted>", self.room_secrets.len()),
            )
            .field("get_budget", &self.get_budget)
            .finish()
    }
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

impl PendingRoomStatus {
    fn is_in_progress(&self) -> bool {
        match self {
            Self::PendingSubscription | Self::Subscribing => true,
            Self::Subscribed | Self::Error(_) => false,
        }
    }
}
