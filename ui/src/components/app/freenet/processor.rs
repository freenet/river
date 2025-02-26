//! Response processing logic for Freenet API

use crate::components::app::freenet::types::{SYNC_STATUS, SyncStatus};
use crate::components::app::room_state_handler;
use crate::invites::PendingInvites;
use crate::room_data::{Rooms, RoomSyncStatus};
use dioxus::prelude::{use_context, Signal, Writable};
use ed25519_dalek::VerifyingKey;
use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::{ContractKey, UpdateData};
use log::{debug, error, info};
use river_common::room_state::ChatRoomStateV1;

/// Process a GetResponse from the Freenet network
pub fn process_get_response(key: ContractKey, state: Vec<u8>) {
    info!("Received GetResponse for key: {:?}", key);
    debug!("Response state size: {} bytes", state.len());

    // Update rooms with received state
    if let Ok(room_state) = ciborium::from_reader::<ChatRoomStateV1, &[u8]>(state.as_ref()) {
        debug!("Successfully deserialized room state");
        let mut rooms = use_context::<Signal<Rooms>>();
        let mut pending_invites = use_context::<Signal<PendingInvites>>();

        // Try to find the room owner from the key
        let key_bytes: [u8; 32] = key.id().as_bytes().try_into().expect("Invalid key length");
        if let Ok(room_owner) = VerifyingKey::from_bytes(&key_bytes) {
            info!("Identified room owner from key: {:?}", room_owner);
            let mut rooms_write = rooms.write();
            let mut pending_write = pending_invites.write();

            // Check if this is a pending invitation
            debug!("Checking if this is a pending invitation");
            let was_pending = room_state_handler::process_room_state_response(
                &mut rooms_write,
                &room_owner,
                room_state.clone(),
                key,
                &mut pending_write
            );

            if was_pending {
                info!("Processed pending invitation for room owned by: {:?}", room_owner);
            }

            if !was_pending {
                // Regular room state update
                info!("Processing regular room state update");
                if let Some(room_data) = rooms_write.map.values_mut().find(|r| r.contract_key == key) {
                    let current_state = room_data.room_state.clone();
                    if let Err(e) = room_data.room_state.merge(
                        &current_state,
                        &room_data.parameters(),
                        &room_state
                    ) {
                        error!("Failed to merge room state: {}", e);
                        *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                        room_data.sync_status = RoomSyncStatus::Error(e);
                    }
                }
            }
        } else {
            error!("Failed to convert key to VerifyingKey");
        }
    } else {
        error!("Failed to decode room state from bytes: {:?}", state.as_slice());
    }
}

/// Process an UpdateNotification from the Freenet network
pub fn process_update_notification(key: ContractKey, update: UpdateData) {
    info!("Received UpdateNotification for key: {:?}", key);
    // Handle incremental updates
    let mut rooms = use_context::<Signal<Rooms>>();
    let mut rooms = rooms.write();
    let key_bytes: [u8; 32] = key.id().as_bytes().try_into().expect("Invalid key length");
    if let Some(room_data) = rooms.map.get_mut(&VerifyingKey::from_bytes(&key_bytes).expect("Invalid key bytes")) {
        debug!("Processing delta update for room");
        if let Ok(delta) = ciborium::from_reader(update.unwrap_delta().as_ref()) {
            debug!("Successfully deserialized delta");
            let current_state = room_data.room_state.clone();
            if let Err(e) = room_data.room_state.apply_delta(
                &current_state,
                &room_data.parameters(),
                &Some(delta)
            ) {
                error!("Failed to apply delta: {}", e);
                *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                room_data.sync_status = RoomSyncStatus::Error(e);
            }
        }
    }
}

/// Process an OK response from the Freenet network
pub fn process_ok_response() {
    info!("Received OK response from host");
    *SYNC_STATUS.write() = SyncStatus::Connected;
    // Update room status to Subscribed when subscription succeeds
    let mut rooms = use_context::<Signal<Rooms>>();
    let mut rooms = rooms.write();
    for room in rooms.map.values_mut() {
        if matches!(room.sync_status, RoomSyncStatus::Subscribing) {
            info!("Room subscription confirmed for: {:?}", room.owner_vk);
            room.sync_status = RoomSyncStatus::Subscribed;
        }
    }
}
//! Response processing logic for Freenet API

use crate::components::app::freenet::types::{SYNC_STATUS, SyncStatus};
use crate::components::app::room_state_handler;
use crate::invites::PendingInvites;
use crate::room_data::{Rooms, RoomSyncStatus};
use dioxus::prelude::{use_context, Signal, Writable};
use ed25519_dalek::VerifyingKey;
use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::{ContractKey, UpdateData};
use log::{debug, error, info};
use river_common::room_state::ChatRoomStateV1;

/// Process a GetResponse from the Freenet network
pub fn process_get_response(key: ContractKey, state: Vec<u8>) {
    info!("Received GetResponse for key: {:?}", key);
    debug!("Response state size: {} bytes", state.len());

    // Update rooms with received state
    if let Ok(room_state) = ciborium::from_reader::<ChatRoomStateV1, &[u8]>(state.as_ref()) {
        debug!("Successfully deserialized room state");
        let mut rooms = use_context::<Signal<Rooms>>();
        let mut pending_invites = use_context::<Signal<PendingInvites>>();

        // Try to find the room owner from the key
        let key_bytes: [u8; 32] = key.id().as_bytes().try_into().expect("Invalid key length");
        if let Ok(room_owner) = VerifyingKey::from_bytes(&key_bytes) {
            info!("Identified room owner from key: {:?}", room_owner);
            let mut rooms_write = rooms.write();
            let mut pending_write = pending_invites.write();

            // Check if this is a pending invitation
            debug!("Checking if this is a pending invitation");
            let was_pending = room_state_handler::process_room_state_response(
                &mut rooms_write,
                &room_owner,
                room_state.clone(),
                key,
                &mut pending_write
            );

            if was_pending {
                info!("Processed pending invitation for room owned by: {:?}", room_owner);
            }

            if !was_pending {
                // Regular room state update
                info!("Processing regular room state update");
                if let Some(room_data) = rooms_write.map.values_mut().find(|r| r.contract_key == key) {
                    let current_state = room_data.room_state.clone();
                    if let Err(e) = room_data.room_state.merge(
                        &current_state,
                        &room_data.parameters(),
                        &room_state
                    ) {
                        error!("Failed to merge room state: {}", e);
                        *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                        room_data.sync_status = RoomSyncStatus::Error(e);
                    }
                }
            }
        } else {
            error!("Failed to convert key to VerifyingKey");
        }
    } else {
        error!("Failed to decode room state from bytes: {:?}", state.as_slice());
    }
}

/// Process an UpdateNotification from the Freenet network
pub fn process_update_notification(key: ContractKey, update: UpdateData) {
    info!("Received UpdateNotification for key: {:?}", key);
    // Handle incremental updates
    let mut rooms = use_context::<Signal<Rooms>>();
    let mut rooms = rooms.write();
    let key_bytes: [u8; 32] = key.id().as_bytes().try_into().expect("Invalid key length");
    if let Some(room_data) = rooms.map.get_mut(&VerifyingKey::from_bytes(&key_bytes).expect("Invalid key bytes")) {
        debug!("Processing delta update for room");
        if let Ok(delta) = ciborium::from_reader(update.unwrap_delta().as_ref()) {
            debug!("Successfully deserialized delta");
            let current_state = room_data.room_state.clone();
            if let Err(e) = room_data.room_state.apply_delta(
                &current_state,
                &room_data.parameters(),
                &Some(delta)
            ) {
                error!("Failed to apply delta: {}", e);
                *SYNC_STATUS.write() = SyncStatus::Error(e.clone());
                room_data.sync_status = RoomSyncStatus::Error(e);
            }
        }
    }
}

/// Process an OK response from the Freenet network
pub fn process_ok_response() {
    info!("Received OK response from host");
    *SYNC_STATUS.write() = SyncStatus::Connected;
    // Update room status to Subscribed when subscription succeeds
    let mut rooms = use_context::<Signal<Rooms>>();
    let mut rooms = rooms.write();
    for room in rooms.map.values_mut() {
        if matches!(room.sync_status, RoomSyncStatus::Subscribing) {
            info!("Room subscription confirmed for: {:?}", room.owner_vk);
            room.sync_status = RoomSyncStatus::Subscribed;
        }
    }
}
