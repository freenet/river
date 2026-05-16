//! In-room direct-message UI (#243 Phase 2, #258 follow-ups).
//!
//! UX model: clicking a member opens the member-info modal, which has a
//! "Send Direct Message" button. That button opens this module's
//! [`DmThreadModal`]: a per-pair thread of decrypted DMs plus a composer.
//!
//! A second, primary entry point lives in the **left rail under Rooms**:
//! [`crate::components::room_list::dm_rail_section::DmRailSection`] lists
//! every open DM thread across ALL rooms the local user is in, with an
//! unread badge per thread. Clicking a row opens [`DmThreadModal`] for
//! that (room, peer). Replaces the earlier per-room inbox button in the
//! members panel (zorolin feedback, 2026-05-16).
//!
//! Persistence model: all message state lives in `ChatRoomStateV1`. This
//! module only adds *view* state — currently open thread, last-seen
//! timestamps per peer for unread tracking. Last-seen state is purely
//! in-memory; reloading the page seeds it from the room state (see
//! [`seed_dm_last_seen_if_needed`]) so previously-read DMs don't pop
//! back as unread on every page load.

mod dm_thread_modal;
mod invite_via_dm_picker_modal;

pub use dm_thread_modal::DmThreadModal;
pub use invite_via_dm_picker_modal::InviteViaDmPickerModal;

use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::chat_delegate::OutboundDmEntry;
use river_core::room_state::direct_messages::PurgeToken;
use river_core::room_state::member::MemberId;
use std::collections::HashMap;

/// Currently-open DM thread, addressed by (room_owner_vk, counterparty).
/// `None` means no DM modal is open.
pub static OPEN_DM_THREAD: GlobalSignal<Option<(VerifyingKey, MemberId)>> = Global::new(|| None);

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

/// In-memory cache of outbound DM plaintext, keyed by
/// `(room_owner_vk, recipient, purge_token)`. Hydrated from the chat
/// delegate on app startup and re-written on every send / purge. Used
/// by [`DmThreadModal`] (and CLI/`riverctl dm list` via the equivalent
/// CLI cache) to render the sender's own outbound bubbles as plaintext
/// instead of "sent — ciphertext only". See issue freenet/river#256.
///
/// Miss on lookup → caller falls back to the placeholder, so DMs sent
/// under older clients (pre-#256) continue to render as ciphertext-only.
pub static OUTBOUND_DMS: GlobalSignal<OutboundDmsCache> = Global::new(OutboundDmsCache::default);

/// In-memory shape of the outbound-DM cache. The on-disk form is
/// `river_core::chat_delegate::OutboundDmStore` (a `Vec` for JSON safety
/// per the bug-prevention pattern); we hold a `HashMap` here for O(1)
/// render-time lookup.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OutboundDmsCache {
    pub by_token: HashMap<(VerifyingKey, MemberId, PurgeToken), OutboundDmEntry>,
}

impl OutboundDmsCache {
    pub fn get(
        &self,
        room: &VerifyingKey,
        recipient: &MemberId,
        token: &PurgeToken,
    ) -> Option<&OutboundDmEntry> {
        self.by_token.get(&(*room, *recipient, *token))
    }
}

/// Open the invite-via-DM picker for the given target peer in the current
/// room.
///
/// Refuses to open for `peer == self` even though all current callers gate
/// on that — defense in depth so a future shortcut doesn't strand a user
/// with a self-DM draft they can't send (Skeptical-review finding #3).
pub fn open_invite_via_dm_picker(current_room: VerifyingKey, peer: MemberId) {
    let Ok(rooms) = crate::components::app::ROOMS.try_read() else {
        return;
    };
    if let Some(room_data) = rooms.map.get(&current_room) {
        let self_id: MemberId = room_data.self_sk.verifying_key().into();
        if self_id == peer {
            dioxus::logger::tracing::warn!(
                "open_invite_via_dm_picker: refusing to open for self-as-peer"
            );
            return;
        }
    }
    drop(rooms);
    crate::util::defer(move || {
        *INVITE_VIA_DM_PICKER.write() = Some((current_room, peer));
    });
}

/// Tracks whether the one-shot DM-last-seen seed has already run for this
/// session. Once set, [`seed_dm_last_seen_if_needed`] is a no-op — which is
/// what we want: if it kept running on every `ROOMS` update, every newly-
/// arrived inbound DM would seed itself and never surface as unread (Codex
/// P2 on #244 review pass 3).
static DM_LAST_SEEN_SEEDED: GlobalSignal<bool> = Global::new(|| false);

/// Pure helper: compute the max inbound DM timestamp per `(room, peer)` in
/// `rooms`. Split from the signal-touching wrapper so it's unit-testable.
pub(crate) fn compute_dm_last_seen(
    rooms: &crate::room_data::Rooms,
) -> HashMap<(VerifyingKey, MemberId), u64> {
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
    updates
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
/// **Subscription semantics.** The intended caller is a `use_effect` that
/// subscribes to [`crate::components::app::ROOMS`] so it fires the FIRST
/// time `ROOMS` hydrates from the delegate (it's empty on synchronous
/// app-component first render). The internal `DM_LAST_SEEN_SEEDED` flag
/// then makes every subsequent call a no-op: if we re-seeded on every
/// `ROOMS` update, a newly-arrived inbound DM would advance the cutoff to
/// its own timestamp and never appear as unread (Codex review #3 caught
/// this).
///
/// Per-`(room, peer)` last-seen is set to the maximum inbound timestamp
/// in that thread; anything newer than the current state still counts as
/// unread.
pub fn seed_dm_last_seen_if_needed() {
    // Cheap early-exit: if we've already seeded once, do nothing.
    if let Ok(g) = DM_LAST_SEEN_SEEDED.try_read() {
        if *g {
            return;
        }
    } else {
        return;
    };

    let Ok(rooms) = crate::components::app::ROOMS.try_read() else {
        return;
    };
    if rooms.map.is_empty() {
        // ROOMS hasn't hydrated yet; wait for the next ROOMS change.
        return;
    }
    let updates = compute_dm_last_seen(&rooms);
    drop(rooms);

    // Latch the seeded flag synchronously so any parallel re-run of this
    // effect (before the deferred write hits) immediately early-exits.
    // Doing the latch BEFORE the deferred write also avoids the
    // one-render-frame "every historical DM looks unread" window the
    // skeptical reviewer (#258 M3) flagged — consumers reading
    // DM_LAST_SEEN_SEEDED see "seeded, write in flight" rather than
    // "not seeded".
    //
    // Safety: a same-tick re-entry doesn't lose the seed because we
    // already computed `updates` from the just-read rooms snapshot and
    // captured it into the defer closure. The flag latch and the write
    // are conceptually one operation.
    crate::util::defer(move || {
        *DM_LAST_SEEN_SEEDED.write() = true;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_data::{RoomData, Rooms};
    use ed25519_dalek::{SigningKey, VerifyingKey};
    use freenet_stdlib::prelude::{ContractCode, ContractKey, Parameters};
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::direct_messages::sign_direct_message;
    use river_core::room_state::member::{AuthorizedMember, Member, MembersV1};
    use river_core::ChatRoomStateV1;

    fn empty_rooms() -> Rooms {
        Rooms {
            map: std::collections::HashMap::new(),
            current_room_key: None,
            migrated_rooms: Vec::new(),
            removed_rooms: std::collections::HashSet::new(),
        }
    }

    fn fixed_sk(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// Build a `Rooms` with one room owned by `owner_sk`, with
    /// `self_sk` as the local user enrolled as a member.
    fn make_rooms(owner_sk: &SigningKey, self_sk: &SigningKey, other_sks: &[&SigningKey]) -> Rooms {
        let owner_vk = owner_sk.verifying_key();
        let owner_id: MemberId = (&owner_vk).into();
        let mut members: Vec<AuthorizedMember> = vec![AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: self_sk.verifying_key(),
            },
            owner_sk,
        )];
        for s in other_sks {
            members.push(AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: owner_id,
                    member_vk: s.verifying_key(),
                },
                owner_sk,
            ));
        }
        let auth_config = AuthorizedConfigurationV1::new(Configuration::default(), owner_sk);
        let state = ChatRoomStateV1 {
            configuration: auth_config,
            members: MembersV1 { members },
            ..Default::default()
        };
        let mut rooms = empty_rooms();
        let contract_code = ContractCode::from(crate::constants::ROOM_CONTRACT_WASM);
        let contract_key = ContractKey::from_params_and_code(
            Parameters::from(crate::util::to_cbor_vec(
                &river_core::room_state::ChatRoomParametersV1 { owner: owner_vk },
            )),
            &contract_code,
        );
        rooms.map.insert(
            owner_vk,
            RoomData {
                owner_vk,
                room_state: state,
                self_sk: self_sk.clone(),
                contract_key,
                last_read_message_id: None,
                secrets: std::collections::HashMap::new(),
                current_secret_version: None,
                last_secret_rotation: None,
                key_migrated_to_delegate: false,
                self_authorized_member: None,
                invite_chain: Vec::new(),
                self_member_info: None,
                previous_contract_key: None,
            },
        );
        rooms
    }

    fn push_dm(
        rooms: &mut Rooms,
        room_owner_vk: &VerifyingKey,
        sender_sk: &SigningKey,
        recipient_vk: &VerifyingKey,
        ts: u64,
    ) {
        let auth = sign_direct_message(
            sender_sk,
            (&sender_sk.verifying_key()).into(),
            recipient_vk.into(),
            room_owner_vk,
            ts,
            b"opaque".to_vec(),
        )
        .expect("sign_direct_message");
        rooms
            .map
            .get_mut(room_owner_vk)
            .unwrap()
            .room_state
            .direct_messages
            .messages
            .push(auth);
    }

    #[test]
    fn compute_dm_last_seen_returns_empty_for_empty_rooms() {
        let updates = compute_dm_last_seen(&empty_rooms());
        assert!(updates.is_empty());
    }

    #[test]
    fn compute_dm_last_seen_only_counts_inbound_to_self() {
        let owner = fixed_sk(1);
        let me = fixed_sk(2);
        let alice = fixed_sk(3);
        let bob = fixed_sk(4);
        let owner_vk = owner.verifying_key();
        let mut rooms = make_rooms(&owner, &me, &[&alice, &bob]);

        // Alice -> me at ts 100; Bob -> me at ts 200; me -> Alice at ts 250
        // (outbound; must NOT contribute).
        push_dm(&mut rooms, &owner_vk, &alice, &me.verifying_key(), 100);
        push_dm(&mut rooms, &owner_vk, &bob, &me.verifying_key(), 200);
        push_dm(&mut rooms, &owner_vk, &me, &alice.verifying_key(), 250);

        let updates = compute_dm_last_seen(&rooms);
        let alice_id: MemberId = (&alice.verifying_key()).into();
        let bob_id: MemberId = (&bob.verifying_key()).into();
        assert_eq!(updates.get(&(owner_vk, alice_id)), Some(&100));
        assert_eq!(updates.get(&(owner_vk, bob_id)), Some(&200));
        // Outbound DM: must NOT seed against my own self_id.
        let me_id: MemberId = (&me.verifying_key()).into();
        assert!(updates.get(&(owner_vk, me_id)).is_none());
    }

    #[test]
    fn compute_dm_last_seen_takes_max_per_peer() {
        let owner = fixed_sk(11);
        let me = fixed_sk(12);
        let alice = fixed_sk(13);
        let owner_vk = owner.verifying_key();
        let mut rooms = make_rooms(&owner, &me, &[&alice]);

        // Three DMs from Alice; the helper must pick the max.
        push_dm(&mut rooms, &owner_vk, &alice, &me.verifying_key(), 100);
        push_dm(&mut rooms, &owner_vk, &alice, &me.verifying_key(), 1_000);
        push_dm(&mut rooms, &owner_vk, &alice, &me.verifying_key(), 500);

        let updates = compute_dm_last_seen(&rooms);
        let alice_id: MemberId = (&alice.verifying_key()).into();
        assert_eq!(updates.get(&(owner_vk, alice_id)), Some(&1_000));
    }
}
