//! In-room direct-message UI (#243 Phase 2).
//!
//! UX model: clicking a member opens the member-info modal, which now has a
//! "Send Direct Message" button. That button opens this module's
//! [`DmThreadModal`]: a per-pair thread of decrypted DMs plus a composer.
//!
//! A second entry point — the "Direct Messages" inbox button in the members
//! panel — opens [`DmInboxModal`], a list of every open DM thread the local
//! user has in the current room with a per-thread unread badge.
//!
//! Persistence model: all message state lives in `ChatRoomStateV1`. This
//! module only adds *view* state — currently open thread, last-seen
//! timestamps per peer for unread tracking. Last-seen state is purely
//! in-memory; reloading the page resets unread counters (acceptable for a
//! first cut, and matches the room-message unread behaviour).

mod dm_inbox_modal;
mod dm_thread_modal;

pub use dm_inbox_modal::DmInboxModal;
pub use dm_thread_modal::DmThreadModal;

use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::MemberId;
use std::collections::HashMap;

/// Currently-open DM thread, addressed by (room_owner_vk, counterparty).
/// `None` means no DM modal is open.
pub static OPEN_DM_THREAD: GlobalSignal<Option<(VerifyingKey, MemberId)>> = Global::new(|| None);

/// `true` when the user clicked the "Direct Messages" button in the members
/// panel and the inbox modal is visible.
pub static DM_INBOX_OPEN: GlobalSignal<bool> = Global::new(|| false);

/// Per-(room, peer) timestamp (unix seconds) of the most recent DM the local
/// user has actually viewed in [`DmThreadModal`]. Anything in
/// `room.direct_messages.messages` addressed to the local user with
/// `timestamp > last_seen` counts as unread.
pub static DM_LAST_SEEN: GlobalSignal<HashMap<(VerifyingKey, MemberId), u64>> =
    Global::new(HashMap::new);

/// Mark every DM from `peer` in `room` as seen up to (and including) the
/// most recent inbound message timestamp known to the synchronizer.
pub fn mark_thread_read(room: VerifyingKey, peer: MemberId, up_to_ts: u64) {
    crate::util::defer(move || {
        DM_LAST_SEEN.with_mut(|seen| {
            let entry = seen.entry((room, peer)).or_insert(0);
            if up_to_ts > *entry {
                *entry = up_to_ts;
            }
        });
    });
}

/// Open the DM thread modal for `(room, peer)`. Closes any other open
/// thread first.
pub fn open_dm_thread(room: VerifyingKey, peer: MemberId) {
    crate::util::defer(move || {
        *OPEN_DM_THREAD.write() = Some((room, peer));
    });
}

/// Open the DM-inbox listing modal for the current room.
pub fn open_dm_inbox() {
    crate::util::defer(move || {
        *DM_INBOX_OPEN.write() = true;
    });
}
