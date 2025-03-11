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

        // I think we could simplify AI!
        for room in ROOMS.read().map.iter() {
            // If the room is not in map then we need to add it and it needs to be subscribed
            if !self.map.contains_key(&room.0) {
                self.register_new_state(*room.0);
                rooms_awaiting_subscription.insert(*room.0, room.1.room_state.clone());
            } else {
                // If the room is disconnected then we need to add it
                if self.map.get(room.0).unwrap().sync_status == RoomSyncStatus::Disconnected {
                    rooms_awaiting_subscription.insert(*room.0, room.1.room_state.clone());
                }
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