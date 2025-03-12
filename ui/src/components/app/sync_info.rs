use std::collections::HashMap;
use dioxus::prelude::{Global, GlobalSignal};
use dioxus::signals::Readable;
use ed25519_dalek::VerifyingKey;
use river_common::ChatRoomStateV1;
use crate::components::app::ROOMS;

pub static SYNC_INFO: GlobalSignal<SyncInfo> = Global::new(|| SyncInfo::new());

pub struct SyncInfo {
    map: HashMap<VerifyingKey, RoomSyncInfo>
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
            map: HashMap::new()
        }
    }

    pub fn register_new_state(&mut self, owner_key: VerifyingKey, last_synced_state : Option<ChatRoomStateV1>) {
        self.map.insert(owner_key, RoomSyncInfo {
            sync_status: RoomSyncStatus::Disconnected,
            last_synced_state
        });
    }

    pub fn rooms_awaiting_subscription(&mut self) -> HashMap<VerifyingKey, ChatRoomStateV1> {
        let mut rooms_awaiting_subscription = HashMap::new();
        let rooms = ROOMS.read();

        for (key, room_data) in rooms.map.iter() {
            // Register new rooms automatically
            if !self.map.contains_key(key) {
                self.register_new_state(*key, Some(room_data.room_state.clone()));
            }
            
            // Add room to awaiting list if it's disconnected
            if self.map.get(key).unwrap().sync_status == RoomSyncStatus::Disconnected {
                rooms_awaiting_subscription.insert(*key, room_data.room_state.clone());
            }
        }

        rooms_awaiting_subscription
    }

    /// Returns a list of rooms for which an update should be sent to the network,
    /// afterwards should call state_updated() for each to register that the state
    /// has been sent
    pub fn needs_to_send_update(&mut self) -> HashMap<VerifyingKey, ChatRoomStateV1> {
        let mut rooms_needing_update = HashMap::new();
        let rooms = ROOMS.read();

        for (key, room_data) in rooms.map.iter() {
            // Register new rooms automatically
            if !self.map.contains_key(key) {
                self.register_new_state(*key);
            }

            // Add room to update list if it's subscribed and the state has changed
            if self.map.get(key).unwrap().sync_status == RoomSyncStatus::Subscribed &&
                self.map.get(key).unwrap().last_synced_state != Some(room_data.room_state.clone()) {
                rooms_needing_update.insert(*key, room_data.room_state.clone());
            }
        }

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
