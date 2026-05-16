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
mod invite_via_dm_picker_modal;

pub use dm_inbox_modal::DmInboxModal;
pub use dm_thread_modal::DmThreadModal;
pub use invite_via_dm_picker_modal::InviteViaDmPickerModal;

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

/// "Share an invite via DM…" picker state. When `Some((room, peer))`, the
/// [`InviteViaDmPickerModal`] is visible and offers to generate an invite
/// for ANOTHER room and pre-fill a DM to `peer` in `room` with the invite
/// URL. See issue #252.
pub static INVITE_VIA_DM_PICKER: GlobalSignal<Option<(VerifyingKey, MemberId)>> =
    Global::new(|| None);

/// Body to pre-fill into the DM composer the next time
/// [`DmThreadModal`] renders for the matching (room, peer). Used by the
/// "Share an invite via DM…" flow (#252) to drop an invite URL straight
/// into the recipient's thread composer.
///
/// Consumed on render: the thread body component drains this signal the
/// first time it matches its `(room, peer)` props, then clears it so a
/// second render doesn't reset what the user has subsequently typed.
pub static DM_DRAFT: GlobalSignal<Option<(VerifyingKey, MemberId, String)>> = Global::new(|| None);

/// Open the invite-via-DM picker for the given target peer in the current
/// room.
pub fn open_invite_via_dm_picker(current_room: VerifyingKey, peer: MemberId) {
    crate::util::defer(move || {
        *INVITE_VIA_DM_PICKER.write() = Some((current_room, peer));
    });
}

/// Initialise [`DM_LAST_SEEN`] from current room state so previously-existing
/// inbound DMs don't show up as "unread" every time the page reloads.
///
/// `DM_LAST_SEEN` is in-memory only — that's an explicit limitation of the
/// first cut, documented in the module header. Without this seeding step,
/// every reload would mark every DM ever received as unread until the user
/// opened each thread, which is much noisier than the room-message badge
/// (which is durable via `last_read_message_id`).
///
/// Called once at app startup AFTER `ROOMS` has been hydrated from the
/// delegate. Per-`(room, peer)` last-seen is set to the maximum inbound
/// timestamp in that thread, so anything newer than the current state
/// (i.e. DMs that arrive while the app is running) still counts as unread.
///
/// Safe to call multiple times — the underlying [`mark_thread_read`] is
/// monotonic and only advances the cutoff forward.
pub fn seed_dm_last_seen_from_rooms() {
    let Ok(rooms) = crate::components::app::ROOMS.try_read() else {
        return;
    };
    let mut updates: HashMap<(VerifyingKey, MemberId), u64> = HashMap::new();
    for (owner_vk, room_data) in &rooms.map {
        let self_id: MemberId = room_data.self_sk.verifying_key().into();
        for msg in &room_data.room_state.direct_messages.messages {
            if msg.message.recipient != self_id {
                continue;
            }
            let key = (*owner_vk, msg.message.sender);
            let entry = updates.entry(key).or_insert(0);
            if msg.message.timestamp > *entry {
                *entry = msg.message.timestamp;
            }
        }
    }
    drop(rooms);
    if updates.is_empty() {
        return;
    }
    crate::util::defer(move || {
        DM_LAST_SEEN.with_mut(|seen| {
            for (key, ts) in updates {
                let entry = seen.entry(key).or_insert(0);
                if ts > *entry {
                    *entry = ts;
                }
            }
        });
    });
}
