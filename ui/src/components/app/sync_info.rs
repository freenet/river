use crate::components::app::freenet_api::constants::REPUT_DELAY_MS;
use crate::components::app::ROOMS;
use crate::util::owner_vk_to_contract_key;
use dioxus::logger::tracing::{debug, warn};
use dioxus::prelude::{Global, GlobalSignal, ReadableExt};
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::tracing::info;
use freenet_stdlib::prelude::ContractInstanceId;
use river_core::room_state::member::MemberId;
use river_core::ChatRoomStateV1;
use std::collections::HashMap;

/// Get current time in milliseconds (works in WASM)
pub(crate) fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0)
    }
}

pub static SYNC_INFO: GlobalSignal<SyncInfo> = Global::new(SyncInfo::new);

/// How many times a room's initial sync may fail before it is promoted to a
/// terminal [`RoomSyncStatus::Error`] instead of being retried forever
/// (freenet/river#290).
///
/// A restored/imported room whose contract is genuinely absent from the
/// network never settles: each GET fails, the room is reset to
/// `Disconnected` (or its `Subscribing` times out), and the next
/// `ProcessRooms` cycle re-GETs it — spinning the "Syncing room state…"
/// indicator forever. Bounding the retries lets the UI surface a terminal
/// error instead.
///
/// The bound is deliberately generous so it does NOT regress the
/// freenet-core#1470 contract-creation race (a freshly-PUT contract that is
/// briefly "not found" while creation completes), which resolves within a
/// couple of retries.
pub const MAX_SYNC_ATTEMPTS_BEFORE_ERROR: u32 = 10;

/// Message shown (and stored in [`RoomSyncStatus::Error`]) when a room's
/// contract could not be found on the network after the retry bound.
pub const CONTRACT_NOT_FOUND_ERROR: &str =
    "This room could not be found on the network — it may have been removed.";

pub struct SyncInfo {
    map: HashMap<VerifyingKey, RoomSyncInfo>,
    instances: HashMap<ContractInstanceId, VerifyingKey>,
}

pub struct RoomSyncInfo {
    pub sync_status: RoomSyncStatus,
    // TODO: Would be better if state implemented Hash trait and just store
    //       a hash of the state
    pub last_synced_state: Option<ChatRoomStateV1>,
    /// Timestamp (in ms) when subscription was initiated, used for timeout detection
    pub subscribing_since: Option<f64>,
    /// How many initial-sync attempts have failed for this room. Reset to 0
    /// once the room reaches [`RoomSyncStatus::Subscribed`]. Used to bound the
    /// otherwise-infinite retry loop for a room whose contract is absent from
    /// the network (freenet/river#290).
    pub failed_sync_attempts: u32,
}

impl SyncInfo {
    pub fn new() -> Self {
        SyncInfo {
            map: HashMap::new(),
            instances: HashMap::new(),
        }
    }

    pub fn register_new_room(&mut self, owner_key: VerifyingKey) {
        let contract_key = owner_vk_to_contract_key(&owner_key);
        let contract_id = contract_key.id();

        if let std::collections::hash_map::Entry::Vacant(e) = self.map.entry(owner_key) {
            debug!(
                "Registering new room with owner key: {:?}, contract ID: {}",
                MemberId::from(owner_key),
                contract_id
            );

            e.insert(RoomSyncInfo {
                sync_status: RoomSyncStatus::Disconnected,
                last_synced_state: None,
                subscribing_since: None,
                failed_sync_attempts: 0,
            });

            self.instances.insert(*contract_id, owner_key);
            debug!(
                "Added mapping from contract ID {} to owner key {:?}",
                contract_id,
                MemberId::from(owner_key)
            );
        } else {
            debug!(
                "Room with owner key {:?} already registered",
                MemberId::from(owner_key)
            );
        }
    }

    pub fn update_sync_status(&mut self, owner_key: &VerifyingKey, status: RoomSyncStatus) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            // Track when subscription starts for timeout detection
            if status == RoomSyncStatus::Subscribing {
                sync_info.subscribing_since = Some(now_ms());
            } else {
                sync_info.subscribing_since = None;
            }
            // A successful subscription clears the failed-attempt budget so a
            // later transient outage doesn't inherit a near-exhausted counter
            // and give up prematurely (freenet/river#290).
            if status == RoomSyncStatus::Subscribed {
                sync_info.failed_sync_attempts = 0;
            }
            sync_info.sync_status = status;
        }
    }

    /// Record a failed initial-sync attempt for a room and decide whether to
    /// keep retrying or give up (freenet/river#290).
    ///
    /// Increments the room's `failed_sync_attempts`. If the count has reached
    /// [`MAX_SYNC_ATTEMPTS_BEFORE_ERROR`], the room is promoted to the terminal
    /// [`RoomSyncStatus::Error`] and `false` is returned (stop retrying);
    /// otherwise the room is reset to [`RoomSyncStatus::Disconnected`] so the
    /// next `ProcessRooms` cycle retries it, and `true` is returned.
    ///
    /// Returns `true` if the caller should schedule a retry, `false` if the
    /// room has been given up on. A room not present in the map (already
    /// removed) returns `false`.
    pub fn record_failed_sync_attempt(&mut self, owner_key: &VerifyingKey) -> bool {
        let Some(sync_info) = self.map.get_mut(owner_key) else {
            return false;
        };
        sync_info.failed_sync_attempts = sync_info.failed_sync_attempts.saturating_add(1);
        if sync_info.failed_sync_attempts >= MAX_SYNC_ATTEMPTS_BEFORE_ERROR {
            warn!(
                "Room {:?} failed initial sync {} times — giving up (contract absent from network)",
                MemberId::from(*owner_key),
                sync_info.failed_sync_attempts
            );
            sync_info.subscribing_since = None;
            sync_info.sync_status = RoomSyncStatus::Error(CONTRACT_NOT_FOUND_ERROR.to_string());
            false
        } else {
            sync_info.subscribing_since = None;
            sync_info.sync_status = RoomSyncStatus::Disconnected;
            true
        }
    }

    /// Refresh `subscribing_since` for a room that is still `Subscribing`,
    /// without otherwise changing its status. A backward-probe recovery
    /// (freenet/river#292) can run longer than `REPUT_DELAY_MS`; calling this
    /// each probe hop keeps `rooms_awaiting_subscription` from reclaiming the
    /// room mid-probe. Unlike `update_sync_status(.., Subscribing)` this does
    /// NOT force the status, so a room that genuinely reached `Subscribed`
    /// mid-probe is left alone (and is not swept anyway).
    pub fn touch_subscribing_since(&mut self, owner_key: &VerifyingKey) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            if sync_info.sync_status == RoomSyncStatus::Subscribing {
                sync_info.subscribing_since = Some(now_ms());
            }
        }
    }

    pub fn update_last_synced_state(&mut self, owner_key: &VerifyingKey, state: &ChatRoomStateV1) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            sync_info.last_synced_state = Some(state.clone());
        }
    }

    pub fn get_sync_status(&self, owner_key: &VerifyingKey) -> Option<&RoomSyncStatus> {
        self.map.get(owner_key).map(|info| &info.sync_status)
    }

    pub fn get_owner_vk_for_instance_id(
        &self,
        instance_id: &ContractInstanceId,
    ) -> Option<VerifyingKey> {
        let result = self.instances.get(instance_id).copied();
        if result.is_some() {
            debug!("Found owner key for contract ID {}", instance_id);
        } else {
            debug!("No owner key found for contract ID {}", instance_id);
            // Log all known mappings to help debug
            for (id, vk) in &self.instances {
                debug!(
                    "Known mapping: contract ID {} -> owner key {:?}",
                    id,
                    MemberId::from(*vk)
                );
            }
        }
        result
    }

    pub fn rooms_awaiting_subscription(&mut self) -> HashMap<VerifyingKey, ChatRoomStateV1> {
        let mut rooms_awaiting_subscription = HashMap::new();
        // Use try_read() to avoid panic when ROOMS is mutably borrowed.
        // This can happen because Dioxus's write guard Drop notifies subscribers
        // synchronously, which can trigger process_rooms() while the write guard
        // is still being dropped. If ROOMS is borrowed, we skip this cycle —
        // we'll be called again on the next ProcessRooms.
        let Ok(rooms) = ROOMS.try_read() else {
            debug!("ROOMS is currently borrowed, skipping rooms_awaiting_subscription");
            return rooms_awaiting_subscription;
        };
        let current_time = now_ms();

        // Rooms whose `Subscribing` status has timed out. Collected here
        // because we can't mutate `self.map` while iterating it below; the
        // timeout counts as a failed sync attempt (freenet/river#290).
        let mut timed_out: Vec<VerifyingKey> = Vec::new();

        for (key, room_data) in rooms.map.iter() {
            // Register new rooms automatically
            if !self.map.contains_key(key) {
                self.register_new_room(*key);
                self.update_last_synced_state(key, &room_data.room_state);
            }

            let sync_info = self.map.get(key).unwrap();

            // Add room to awaiting list if it's disconnected
            if sync_info.sync_status == RoomSyncStatus::Disconnected {
                rooms_awaiting_subscription.insert(*key, room_data.room_state.clone());
            }

            // Check for subscription timeout - if subscribing for longer than REPUT_DELAY_MS,
            // count a failed attempt and (unless the retry bound is reached) re-PUT.
            if sync_info.sync_status == RoomSyncStatus::Subscribing {
                if let Some(started_at) = sync_info.subscribing_since {
                    let elapsed_ms = current_time - started_at;
                    if elapsed_ms >= REPUT_DELAY_MS as f64 {
                        warn!(
                            "Subscription timeout for room {:?} after {:.1}s - will re-PUT contract",
                            MemberId::from(*key),
                            elapsed_ms / 1000.0
                        );
                        timed_out.push(*key);
                    }
                }
            }
        }

        // Now record the failure for timed-out rooms. `record_failed_sync_attempt`
        // resets the room to `Disconnected` (retry) or, once the bound is hit,
        // promotes it to terminal `Error` — in which case it must NOT be re-added
        // to the awaiting set, so the spinner can stop (freenet/river#290).
        for key in timed_out {
            let should_retry = self.record_failed_sync_attempt(&key);
            if should_retry {
                if let Some(room_data) = rooms.map.get(&key) {
                    rooms_awaiting_subscription.insert(key, room_data.room_state.clone());
                }
            }
        }

        rooms_awaiting_subscription
    }

    /// Returns rooms needing sync: current state + last synced baseline (if any).
    /// The baseline is used by the caller to compute a delta instead of sending full state.
    pub fn needs_to_send_update(
        &mut self,
    ) -> HashMap<VerifyingKey, (ChatRoomStateV1, Option<ChatRoomStateV1>)> {
        let mut rooms_needing_update = HashMap::new();

        // FIXME: Temporarily disabled to fix infinite loop bug
        // This secret rotation/generation code was modifying ROOMS inside a "check if sync needed" function,
        // which triggered use_effect → ProcessRooms → needs_to_send_update → ROOMS.with_mut → use_effect (infinite loop)
        //
        // TODO: Move this logic to a separate periodic task that runs independently of sync triggers
        // See: https://github.com/freenet/river/issues/XXX
        //
        // let keys_to_process: Vec<VerifyingKey> = ROOMS.read().map.keys().copied().collect();
        //
        // for key in &keys_to_process {
        //     let should_generate = ROOMS.read().map.get(key).map(|room_data| {
        //         room_data.owner_vk == room_data.self_sk.verifying_key()
        //     }).unwrap_or(false);
        //
        //     if should_generate {
        //         ROOMS.with_mut(|rooms| {
        //             if let Some(room_data) = rooms.map.get_mut(key) {
        //                 if room_data.needs_secret_rotation() { ... }
        //                 if let Some(secrets_delta) = room_data.generate_missing_member_secrets() { ... }
        //             }
        //         });
        //     }
        // }

        // Second pass: check which rooms need updates
        let Ok(rooms) = ROOMS.try_read() else {
            debug!("ROOMS is currently borrowed, skipping needs_to_send_update");
            return rooms_needing_update;
        };

        debug!(
            "Checking for rooms that need updates, total rooms: {}",
            rooms.map.len()
        );

        for (key, room_data) in rooms.map.iter() {
            // Register new rooms automatically
            if !self.map.contains_key(key) {
                info!("Registering new room: {:?}", key);
                self.register_new_room(*key);
            }

            let sync_info = self.map.get(key).unwrap();
            let sync_status = &sync_info.sync_status;
            let has_last_synced = sync_info.last_synced_state.is_some();
            let states_match = sync_info.last_synced_state.as_ref() == Some(&room_data.room_state);

            debug!(
                "Room {:?} - sync status: {:?}, has last synced: {}, states match: {}",
                MemberId::from(key),
                sync_status,
                has_last_synced,
                states_match
            );

            if let Some(last_state) = &sync_info.last_synced_state {
                debug!(
                    "Room {:?} - last synced: {} members/{} member_info, current: {} members/{} member_info",
                    MemberId::from(key),
                    last_state.members.members.len(),
                    last_state.member_info.member_info.len(),
                    room_data.room_state.members.members.len(),
                    room_data.room_state.member_info.member_info.len(),
                );
            }

            // Add room to update list if it's subscribed and the state has changed
            if *sync_status == RoomSyncStatus::Subscribed {
                if !states_match {
                    info!(
                        "Room {:?} needs update - state has changed",
                        MemberId::from(key)
                    );
                    rooms_needing_update.insert(
                        *key,
                        (
                            room_data.room_state.clone(),
                            sync_info.last_synced_state.clone(),
                        ),
                    );
                    // Don't update the last synced state here - it will be updated after successful network send
                } else {
                    debug!(
                        "Room {:?} doesn't need update - state unchanged",
                        MemberId::from(key)
                    );
                }
            } else {
                debug!(
                    "Room {:?} doesn't need update - not subscribed (status: {:?})",
                    MemberId::from(key),
                    sync_status
                );
            }
        }

        info!("Found {} rooms needing updates", rooms_needing_update.len());
        rooms_needing_update
    }

    /// Register that the state's current value has been sent to the network
    pub fn state_updated(&mut self, owner_key: &VerifyingKey, new_state: ChatRoomStateV1) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            sync_info.last_synced_state = Some(new_state);
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum RoomSyncStatus {
    Disconnected,

    Subscribing,

    Subscribed,

    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn test_owner(seed: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    /// `touch_subscribing_since` refreshes the timestamp for a `Subscribing`
    /// room — this is what keeps a long backward-probe recovery out of the
    /// `rooms_awaiting_subscription` timeout sweep (freenet/river#292) — but
    /// must NOT touch a room in any other status (no spurious status flap,
    /// and a `Subscribed` room is not swept anyway).
    #[test]
    fn touch_subscribing_since_only_restamps_while_subscribing() {
        let mut si = SyncInfo::new();
        let owner = test_owner(1);
        si.register_new_room(owner);

        // Subscribing room with a cleared timestamp → touch re-stamps it.
        si.update_sync_status(&owner, RoomSyncStatus::Subscribing);
        si.map.get_mut(&owner).unwrap().subscribing_since = None;
        si.touch_subscribing_since(&owner);
        assert!(
            si.map.get(&owner).unwrap().subscribing_since.is_some(),
            "touch must re-stamp a Subscribing room"
        );

        // Subscribed room → touch must NOT re-stamp and must NOT flip status.
        si.update_sync_status(&owner, RoomSyncStatus::Subscribed);
        assert!(si.map.get(&owner).unwrap().subscribing_since.is_none());
        si.touch_subscribing_since(&owner);
        assert!(
            si.map.get(&owner).unwrap().subscribing_since.is_none(),
            "touch must not act on a non-Subscribing room"
        );
        assert_eq!(
            si.map.get(&owner).unwrap().sync_status,
            RoomSyncStatus::Subscribed,
            "touch must never change the status"
        );
    }

    /// freenet/river#290: a room whose contract is absent from the network
    /// must NOT retry forever. After `MAX_SYNC_ATTEMPTS_BEFORE_ERROR` failed
    /// attempts it is promoted to a terminal `Error` and `record_failed_sync_attempt`
    /// returns `false` (stop retrying). Up to that point it stays `Disconnected`
    /// (retry) so the generous bound preserves the freenet-core#1470 race
    /// handling.
    #[test]
    fn record_failed_sync_attempt_bounds_retries_then_errors() {
        let mut si = SyncInfo::new();
        let owner = test_owner(7);
        si.register_new_room(owner);

        // All but the last attempt: keep retrying, status stays Disconnected.
        for attempt in 1..MAX_SYNC_ATTEMPTS_BEFORE_ERROR {
            let should_retry = si.record_failed_sync_attempt(&owner);
            assert!(should_retry, "attempt {attempt} (< bound) must still retry");
            assert_eq!(
                si.map.get(&owner).unwrap().sync_status,
                RoomSyncStatus::Disconnected,
                "attempt {attempt} must reset to Disconnected for retry"
            );
            assert_eq!(si.map.get(&owner).unwrap().failed_sync_attempts, attempt);
        }

        // The bound-th attempt gives up: terminal Error, stop retrying.
        let should_retry = si.record_failed_sync_attempt(&owner);
        assert!(!should_retry, "reaching the bound must stop retrying");
        assert!(
            matches!(si.get_sync_status(&owner), Some(RoomSyncStatus::Error(_))),
            "reaching the bound must promote the room to terminal Error"
        );
        assert!(
            si.map.get(&owner).unwrap().subscribing_since.is_none(),
            "giving up must clear the subscribing timestamp"
        );

        // Further failures keep the room terminal and never re-arm a retry.
        assert!(!si.record_failed_sync_attempt(&owner));
        assert!(matches!(
            si.get_sync_status(&owner),
            Some(RoomSyncStatus::Error(_))
        ));
    }

    /// A successful subscription clears the failed-attempt budget so a later
    /// transient failure starts counting from zero again rather than tipping a
    /// long-lived room straight into the terminal Error state.
    #[test]
    fn successful_subscription_resets_failure_budget() {
        let mut si = SyncInfo::new();
        let owner = test_owner(9);
        si.register_new_room(owner);

        // Burn most of the budget without reaching the bound.
        for _ in 1..MAX_SYNC_ATTEMPTS_BEFORE_ERROR {
            assert!(si.record_failed_sync_attempt(&owner));
        }
        assert_eq!(
            si.map.get(&owner).unwrap().failed_sync_attempts,
            MAX_SYNC_ATTEMPTS_BEFORE_ERROR - 1
        );

        // A successful sync resets the counter.
        si.update_sync_status(&owner, RoomSyncStatus::Subscribed);
        assert_eq!(si.map.get(&owner).unwrap().failed_sync_attempts, 0);

        // So the next failure starts over and does NOT immediately error.
        assert!(si.record_failed_sync_attempt(&owner));
        assert_eq!(
            si.map.get(&owner).unwrap().sync_status,
            RoomSyncStatus::Disconnected
        );
    }

    /// `record_failed_sync_attempt` on an unknown room is a no-op that reports
    /// "do not retry" rather than panicking.
    #[test]
    fn record_failed_sync_attempt_on_unknown_room_is_noop() {
        let mut si = SyncInfo::new();
        let owner = test_owner(11);
        assert!(!si.record_failed_sync_attempt(&owner));
        assert!(si.get_sync_status(&owner).is_none());
    }
}
