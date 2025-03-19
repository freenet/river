use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::Readable;
use freenet_stdlib::prelude::ContractKey;
use river_common::room_state::member::MemberId;

pub fn handle_subscribe_response(key: ContractKey, subscribed: bool) {
    info!("Received subscribe response for key {key}, subscribed: {subscribed}");

    // Get the owner VK for this contract first, then release the read lock
    let owner_vk_opt = {
        let sync_info = SYNC_INFO.read();
        sync_info
            .get_owner_vk_for_instance_id(&key.id())
            .map(|vk| vk)
    };

    if let Some(owner_vk) = owner_vk_opt {
        if subscribed {
            info!(
                "Successfully subscribed to contract for room: {:?}",
                MemberId::from(owner_vk)
            );

            // Update the sync status to subscribed in a separate block
            SYNC_INFO
                .write()
                .update_sync_status(&owner_vk, RoomSyncStatus::Subscribed);
        } else {
            warn!("Failed to subscribe to contract: {}", key.id());
            // Update the sync status to error
            SYNC_INFO.write().update_sync_status(
                &owner_vk,
                RoomSyncStatus::Error("Subscription failed".to_string()),
            );
        }
    } else {
        warn!("Could not find owner VK for contract ID: {}", key.id());
    }
}
