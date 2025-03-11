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

    pub fn register_new_state(&mut self, owner_key: VerifyingKey) {
        self.map.insert(owner_key, RoomSyncInfo {
            sync_status: RoomSyncStatus::Disconnected,
            last_synced_state: None
        });
    }

    pub fn rooms_awaiting_subscription(&mut self) -> HashMap<VerifyingKey, ChatRoomStateV1> {
        let mut rooms_awaiting_subscription = HashMap::new();
        let rooms = ROOMS.read();

        for (key, room_data) in rooms.map.iter() {
            // Register new rooms automatically
            if !self.map.contains_key(key) {
                self.register_new_state(*key);
            }
            
            // Add room to awaiting list if it's disconnected
            if !self.map.contains_key(key) || 
               self.map.get(key).unwrap().sync_status == RoomSyncStatus::Disconnected {
                rooms_awaiting_subscription.insert(*key, room_data.room_state.clone());
            }
        }

        rooms_awaiting_subscription
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum RoomSyncStatus {
    Disconnected,

    Subscribing,

    Subscribed,

    Error(String),
}
