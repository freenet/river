use std::collections::HashMap;
use dioxus::logger::tracing::info;
use dioxus::prelude::{Global, GlobalSignal};
use dioxus::signals::Readable;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::ContractInstanceId;
use river_common::ChatRoomStateV1;
use crate::components::app::ROOMS;
use crate::util::owner_vk_to_contract_key;

pub static SYNC_INFO: GlobalSignal<SyncInfo> = Global::new(|| SyncInfo::new());

pub struct SyncInfo {
    map: HashMap<VerifyingKey, RoomSyncInfo>,
    instances: HashMap<ContractInstanceId, VerifyingKey>,
}

pub struct RoomSyncInfo {
    pub sync_status: RoomSyncStatus,
    // TODO: Would be better if state implemented Hash trait and just store
    //       a hash of the state
    pub last_synced_state: Option<ChatRoomStateV1>,
}

impl SyncInfo {
    pub fn new() -> Self {
        SyncInfo {
            map: HashMap::new(),
            instances: HashMap::new(),
        }
    }

    pub fn register_new_room(&mut self, owner_key: VerifyingKey) {
        if !self.map.contains_key(&owner_key) {
            self.map.insert(owner_key, RoomSyncInfo {
                sync_status: RoomSyncStatus::Disconnected,
                last_synced_state: None,
            });
            let contract_key = owner_vk_to_contract_key(&owner_key);
            self.instances.insert(*contract_key.id(), owner_key);
        }
    }

    pub fn update_sync_status(&mut self, owner_key: &VerifyingKey, status: RoomSyncStatus) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            sync_info.sync_status = status;
        }
    }

    pub fn update_last_synced_state(&mut self, owner_key: &VerifyingKey, state: &ChatRoomStateV1) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            sync_info.last_synced_state = Some(state.clone());
        }
    }

    pub fn get_room_vk_for_instance_id(&self, instance_id: &ContractInstanceId) -> Option<VerifyingKey> {
        self.instances.get(instance_id).copied()
    }

    pub fn rooms_awaiting_subscription(&mut self) -> HashMap<VerifyingKey, ChatRoomStateV1> {
        let mut rooms_awaiting_subscription = HashMap::new();
        let rooms = ROOMS.read();

        for (key, room_data) in rooms.map.iter() {
            // Register new rooms automatically
            if !self.map.contains_key(key) {
                self.register_new_room(*key);
                self.update_last_synced_state(key, &room_data.room_state);
            }
            
            // Add room to awaiting list if it's disconnected
            if self.map.get(key).unwrap().sync_status == RoomSyncStatus::Disconnected {
                rooms_awaiting_subscription.insert(*key, room_data.room_state.clone());
            }
        }

        rooms_awaiting_subscription
    }

    /// Returns a list of rooms for which an update should be sent to the network,
    /// automatically updates the last_synced_state for each room
    pub fn needs_to_send_update(&mut self) -> HashMap<VerifyingKey, ChatRoomStateV1> {
        let mut rooms_needing_update = HashMap::new();
        let rooms = ROOMS.read();

        info!("Checking for rooms that need updates, total rooms: {}", rooms.map.len());
        
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
            
            info!(
                "Room {:?} - sync status: {:?}, has last synced: {}, states match: {}", 
                key, sync_status, has_last_synced, states_match
            );

            // Add room to update list if it's subscribed and the state has changed
            if *sync_status == RoomSyncStatus::Subscribed {
                if !states_match {
                    info!("Room {:?} needs update - state has changed", key);
                    rooms_needing_update.insert(*key, room_data.room_state.clone());
                    // Update the last synced state immediately to avoid duplicate updates
                    self.state_updated(key, room_data.room_state.clone());
                } else {
                    info!("Room {:?} doesn't need update - state unchanged", key);
                }
            } else {
                info!("Room {:?} doesn't need update - not subscribed (status: {:?})", key, sync_status);
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
