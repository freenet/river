use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::ROOMS;
use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::ReadableExt;
use freenet_stdlib::prelude::ContractKey;
use river_core::room_state::member::MemberId;

/// Handle a subscribe response from the network.
/// Returns `true` if a re-PUT should be scheduled (subscription failed but we have local state).
pub fn handle_subscribe_response(key: ContractKey, subscribed: bool) -> bool {
    info!("Received subscribe response for key {key}, subscribed: {subscribed}");

    // Get the owner VK for this contract first, then release the read lock
    let owner_vk_opt = {
        let sync_info = SYNC_INFO.read();
        sync_info.get_owner_vk_for_instance_id(key.id())
    };

    if let Some(owner_vk) = owner_vk_opt {
        if subscribed {
            info!(
                "Successfully subscribed to contract for room: {:?}",
                MemberId::from(owner_vk)
            );

            // Update the sync status to subscribed in a separate block
            crate::util::defer(move || {
                SYNC_INFO
                    .write()
                    .update_sync_status(&owner_vk, RoomSyncStatus::Subscribed);
            });
            false
        } else {
            warn!("Failed to subscribe to contract: {}", key.id());

            // Check if we have local state for this room
            let has_local_state = ROOMS.read().map.contains_key(&owner_vk);

            if has_local_state {
                // We have local state but the contract doesn't exist on the network.
                // Set status to Disconnected to trigger a re-PUT on the next ProcessRooms cycle.
                info!(
                    "Subscription failed for room {:?} but we have local state - will re-PUT contract",
                    MemberId::from(owner_vk)
                );
                crate::util::defer(move || {
                    let mut sync_info = SYNC_INFO.write();
                    // Retry with the PUT even if this room would otherwise
                    // subscribe with a code-free GET (freenet/river#757).
                    sync_info.require_seed_put(&owner_vk);
                    sync_info.update_sync_status(&owner_vk, RoomSyncStatus::Disconnected);
                });
                true // Signal that a re-PUT should be scheduled
            } else {
                // No local state - this is a genuine error (e.g., trying to join a room that doesn't exist)
                warn!(
                    "Subscription failed for room {:?} and no local state available",
                    MemberId::from(owner_vk)
                );
                crate::util::defer(move || {
                    SYNC_INFO.write().update_sync_status(
                        &owner_vk,
                        RoomSyncStatus::Error(
                            "Subscription failed - contract not found on network".to_string(),
                        ),
                    );
                });
                false
            }
        }
    } else {
        warn!("Could not find owner VK for contract ID: {}", key.id());
        false
    }
}

#[cfg(test)]
mod tests {
    /// freenet/river#757: a refused subscription with local state must put the
    /// room back on the seeding PUT before handing it to `process_rooms`, or a
    /// room on the code-free GET route would just GET again.
    #[test]
    fn refused_subscription_requires_the_seed_put_before_retrying() {
        let src: String = crate::util::strip_comments(
            include_str!("subscribe_response.rs")
                .split("mod tests {")
                .next()
                .unwrap(),
        )
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
        let seed = src
            .find("sync_info.require_seed_put(&owner_vk);")
            .expect("a refused subscription must require the seed PUT");
        let retry = src
            .find("sync_info.update_sync_status(&owner_vk,RoomSyncStatus::Disconnected);")
            .expect("and hand the room back as Disconnected");
        assert!(seed < retry);
    }
}
