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
fn now_ms() -> f64 {
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
            sync_info.sync_status = status;
        }
    }

    pub fn update_last_synced_state(&mut self, owner_key: &VerifyingKey, state: &ChatRoomStateV1) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            sync_info.last_synced_state = Some(state.clone());
        }
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
        let rooms = ROOMS.read();
        let current_time = now_ms();

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
            // reset to Disconnected to trigger a re-PUT
            if sync_info.sync_status == RoomSyncStatus::Subscribing {
                if let Some(started_at) = sync_info.subscribing_since {
                    let elapsed_ms = current_time - started_at;
                    if elapsed_ms >= REPUT_DELAY_MS as f64 {
                        warn!(
                            "Subscription timeout for room {:?} after {:.1}s - will re-PUT contract",
                            MemberId::from(*key),
                            elapsed_ms / 1000.0
                        );
                        // Reset to disconnected to trigger re-PUT
                        // We can't modify the map while iterating, so collect for later
                        rooms_awaiting_subscription.insert(*key, room_data.room_state.clone());
                    }
                }
            }
        }

        // Now update the status for timed-out rooms
        for key in rooms_awaiting_subscription.keys() {
            if let Some(sync_info) = self.map.get_mut(key) {
                if sync_info.sync_status == RoomSyncStatus::Subscribing {
                    sync_info.sync_status = RoomSyncStatus::Disconnected;
                    sync_info.subscribing_since = None;
                }
            }
        }

        rooms_awaiting_subscription
    }

    /// Returns a list of rooms for which an update should be sent to the network,
    /// automatically updates the last_synced_state for each room
    pub fn needs_to_send_update(&mut self) -> HashMap<VerifyingKey, ChatRoomStateV1> {
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
        let rooms = ROOMS.read();

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
                    rooms_needing_update.insert(*key, room_data.room_state.clone());
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
