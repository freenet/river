use crate::components::app::freenet_api::error::SynchronizerError;
use crate::components::app::freenet_api::room_synchronizer::RoomSynchronizer;
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::ROOMS;
use crate::util::owner_vk_to_contract_key;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::Readable;
use freenet_stdlib::prelude::ContractKey;
use river_core::room_state::member::MemberId;

pub async fn handle_put_response(
    room_synchronizer: &mut RoomSynchronizer,
    key: ContractKey,
) -> Result<(), SynchronizerError> {
    let contract_id = key.id();
    info!("Received PutResponse for contract ID: {}", contract_id);

    // Get the owner VK first, then release the read lock
    let owner_vk_opt = {
        let sync_info = SYNC_INFO.read();
        sync_info.get_owner_vk_for_instance_id(contract_id)
    };

    // If not found in SYNC_INFO, try fallback lookup from ROOMS
    // This handles the case where the room creator's SYNC_INFO wasn't properly initialized
    let owner_vk_opt = if owner_vk_opt.is_none() {
        warn!(
            "Owner VK not found in SYNC_INFO for contract ID: {}, trying fallback from ROOMS",
            contract_id
        );

        let rooms = ROOMS.read();
        let mut found_owner_vk = None;

        for owner_key in rooms.map.keys() {
            let room_contract_key = owner_vk_to_contract_key(owner_key);
            if room_contract_key.id() == contract_id {
                info!(
                    "Found matching owner key in ROOMS: {:?}",
                    MemberId::from(*owner_key)
                );
                found_owner_vk = Some(*owner_key);
                break;
            }
        }

        found_owner_vk
    } else {
        owner_vk_opt
    };

    match owner_vk_opt {
        Some(owner_vk) => {
            info!(
                "Found owner VK for contract ID {}: {:?}",
                contract_id,
                MemberId::from(owner_vk)
            );

            // Ensure SYNC_INFO is properly set up for this room before subscribing
            SYNC_INFO.with_mut(|sync_info| {
                sync_info.register_new_room(owner_vk);
            });

            // Now subscribe to the contract
            let subscribe_result = room_synchronizer.subscribe_to_contract(&key).await;

            if let Err(e) = subscribe_result {
                error!("Failed to subscribe to contract after PUT: {}", e);
                // Update the sync status to error
                SYNC_INFO
                    .write()
                    .update_sync_status(&owner_vk, RoomSyncStatus::Error(e.to_string()));
            } else {
                // Update sync status in a separate block to avoid nested borrows
                SYNC_INFO
                    .write()
                    .update_sync_status(&owner_vk, RoomSyncStatus::Subscribed);
            }

            // Log the current state of all rooms after successful PUT
            let rooms_count = {
                let rooms = ROOMS.read();
                rooms.map.len()
            };
            info!("Current rooms count after PutResponse: {}", rooms_count);

            // Get room information in a separate block
            let room_info: Vec<(MemberId, String)> = {
                let rooms = ROOMS.read();
                rooms
                    .map
                    .keys()
                    .map(|room_key| {
                        let contract_key = owner_vk_to_contract_key(room_key);
                        let room_contract_id = contract_key.id();
                        (MemberId::from(*room_key), room_contract_id.to_string())
                    })
                    .collect()
            };

            // Log room information
            for (member_id, contract_id) in room_info {
                info!("Room in map: {:?}, contract ID: {}", member_id, contract_id);
            }
        }
        None => {
            warn!(
                "Warning: Could not find owner VK for contract ID: {}",
                contract_id
            );
        }
    }

    Ok(())
}
