#![allow(dead_code)]

use super::error::SynchronizerError;
use crate::components::app::chat_delegate::save_rooms_to_delegate;
use crate::components::app::document_title::{
    mark_current_room_as_read, update_document_title, DOCUMENT_VISIBLE,
};
use crate::components::app::freenet_api::constants::INVITATION_TIMEOUT_MS;
use crate::components::app::notifications::{mark_initial_sync_complete, notify_new_messages};
use crate::components::app::receive_times::record_receive_times;
use crate::components::app::sync_info::{now_ms, RoomSyncStatus, SYNC_INFO};
use crate::components::app::{CURRENT_ROOM, PENDING_INVITES, ROOMS, WEB_API};
use crate::constants::ROOM_CONTRACT_WASM;
use crate::invites::PendingRoomStatus;
use crate::util::ecies::decrypt_with_symmetric_key;
use crate::util::{owner_vk_to_contract_key, to_cbor_vec};
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use freenet_scaffold::ComposableState;
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest},
    prelude::{
        ContractCode, ContractContainer, ContractInstanceId, ContractKey, ContractWasmAPIVersion,
        Parameters, UpdateData, WrappedContract, WrappedState,
    },
};
use river_core::room_state::member::MemberId;
use river_core::room_state::message::{MessageId, RoomMessageBody};
use river_core::room_state::privacy::PrivacyMode;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::collections::HashMap;
use std::sync::Arc;

/// Identifies contracts that have changed in order to send state updates to Freene
#[derive(Clone)]
pub struct RoomSynchronizer {
    contract_sync_info: HashMap<ContractInstanceId, ContractSyncInfo>,
}

impl RoomSynchronizer {
    pub(crate) fn apply_delta(&self, owner_vk: &VerifyingKey, delta: ChatRoomStateV1Delta) {
        // Extract new messages for notifications before entering the mutable borrow
        let new_messages = delta.recent_messages.clone();

        ROOMS.with_mut(|rooms| {
            if let Some(room_data) = rooms.map.get_mut(owner_vk) {
                let params = ChatRoomParametersV1 { owner: *owner_vk };

                // Log the delta being applied, especially any member_info with versions
                if let Some(member_info) = &delta.member_info {
                    info!("Applying member_info delta with {} items", member_info.len());
                    for info in member_info {
                        info!("Delta contains member_info with version: {} for member: {:?}, nickname: {}",
                              info.member_info.version,
                              info.member_info.member_id,
                              info.member_info.preferred_nickname);
                    }
                }

                // Log current versions before applying delta
                info!("Current member_info state before delta ({} items):",
                      room_data.room_state.member_info.member_info.len());
                for info in &room_data.room_state.member_info.member_info {
                    info!("Current member_info version: {} for member: {:?}, nickname: {}",
                          info.member_info.version,
                          info.member_info.member_id,
                          info.member_info.preferred_nickname);
                }

                // Capture data for notifications before we modify room_data
                let self_member_id: MemberId = room_data.self_sk.verifying_key().into();
                let member_info = room_data.room_state.member_info.clone();
                let room_secrets = room_data.secrets.clone();

                // Clone the state to avoid borrowing issues
                let state_clone = room_data.room_state.clone();

                match room_data
                    .room_state
                    .apply_delta(&state_clone, &params, &Some(delta))
                {
                    Ok(_) => {
                        // For private rooms, rebuild actions_state with decrypted content
                        // (apply_delta only processes public actions)
                        let is_private = room_data.room_state.configuration.configuration.privacy_mode
                            == PrivacyMode::Private;
                        if is_private {
                            // Decrypt all private action messages using version-aware lookup
                            let decrypted_actions: HashMap<MessageId, Vec<u8>> = room_data
                                .room_state
                                .recent_messages
                                .messages
                                .iter()
                                .filter(|msg| msg.message.content.is_action())
                                .filter_map(|msg| {
                                    if let RoomMessageBody::Private { ciphertext, nonce, secret_version, .. } =
                                        &msg.message.content
                                    {
                                        room_data.get_secret_for_version(*secret_version)
                                            .and_then(|secret| {
                                                decrypt_with_symmetric_key(secret, ciphertext, nonce)
                                                    .ok()
                                                    .map(|plaintext| (msg.id(), plaintext))
                                            })
                                    } else {
                                        None
                                    }
                                })
                                .collect();

                            room_data
                                .room_state
                                .recent_messages
                                .rebuild_actions_state_with_decrypted(&decrypted_actions);
                        }

                        // Log versions after applying delta
                        info!("Updated member_info state after delta ({} items):",
                              room_data.room_state.member_info.member_info.len());
                        for info in &room_data.room_state.member_info.member_info {
                            info!("Updated member_info version: {} for member: {:?}, nickname: {}",
                                  info.member_info.version,
                                  info.member_info.member_id,
                                  info.member_info.preferred_nickname);
                        }

                        // Update the last synced state
                        SYNC_INFO
                            .write()
                            .update_last_synced_state(owner_vk, &room_data.room_state);

                        // Notify about new messages from other users
                        if let Some(messages) = new_messages {
                            // Record receive timestamps for propagation delay tracking
                            let msg_ids: Vec<_> = messages.iter().map(|m| m.id()).collect();
                            record_receive_times(&msg_ids);

                            notify_new_messages(
                                owner_vk,
                                &messages,
                                self_member_id,
                                &member_info,
                                &room_secrets,
                            );

                            // If user is viewing this room with tab visible, mark as read immediately
                            let is_visible = *DOCUMENT_VISIBLE.read();
                            let is_current_room = CURRENT_ROOM.read().owner_key == Some(*owner_vk);
                            if is_visible && is_current_room {
                                mark_current_room_as_read();
                            }
                        }

                        // Update document title (may show unread count)
                        update_document_title();

                        // Persist to delegate so state survives refresh
                        wasm_bindgen_futures::spawn_local(async {
                            if let Err(e) = save_rooms_to_delegate().await {
                                error!("Failed to save rooms to delegate after delta: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        error!("Failed to apply delta: {}", e);
                    }
                }
            } else {
                warn!("Room not found in rooms map for apply_delta, ignoring delta");
                // For now, we'll just ignore deltas for rooms we don't have
                // The room should be created through a GET response, not a delta
            }
        });
    }
}

impl RoomSynchronizer {
    pub fn new() -> Self {
        Self {
            contract_sync_info: HashMap::new(),
        }
    }

    /// Send updates to the network for any room that has changed locally
    /// Should be called after modification detected to Signal<Rooms>
    pub async fn process_rooms(&mut self) -> Result<(), SynchronizerError> {
        info!("Processing rooms");

        // Check if WebAPI is available before processing invitations
        // This prevents updating status when we can't actually send requests
        let web_api_available = WEB_API.read().is_some();

        // Reset stuck invitations that have been in Subscribing state too long
        if web_api_available {
            let stuck_invites: Vec<VerifyingKey> = {
                let pending = PENDING_INVITES.read();
                let now = now_ms();
                pending
                    .map
                    .iter()
                    .filter(|(_, join)| {
                        matches!(join.status, PendingRoomStatus::Subscribing)
                            && join
                                .subscribing_since
                                .is_none_or(|since| now - since > INVITATION_TIMEOUT_MS as f64)
                    })
                    .map(|(vk, _)| *vk)
                    .collect()
            };
            for vk in stuck_invites {
                warn!(
                    "Invitation for {:?} stuck in Subscribing, resetting for retry",
                    MemberId::from(vk)
                );
                PENDING_INVITES.with_mut(|pending| {
                    if let Some(join) = pending.map.get_mut(&vk) {
                        join.status = PendingRoomStatus::PendingSubscription;
                        join.subscribing_since = None;
                    }
                });
                SYNC_INFO
                    .write()
                    .update_sync_status(&vk, RoomSyncStatus::Disconnected);
            }
        }

        // First, check for pending invitations that need subscription
        // Collect keys that need subscription without holding the read lock
        let invites_to_subscribe: Vec<VerifyingKey> = if web_api_available {
            let pending_invites = PENDING_INVITES.read();
            pending_invites
                .map
                .iter()
                .filter(|(_, join)| matches!(join.status, PendingRoomStatus::PendingSubscription))
                .map(|(key, _)| *key)
                .collect()
        } else {
            // WebAPI not available, skip invitation processing until connection established
            Vec::new()
        };

        if !invites_to_subscribe.is_empty() {
            info!(
                "Found {} pending invitations to subscribe to",
                invites_to_subscribe.len()
            );

            for owner_vk in invites_to_subscribe {
                info!(
                    "Subscribing to room for invitation: {:?}",
                    MemberId::from(owner_vk)
                );

                let contract_key = owner_vk_to_contract_key(&owner_vk);

                // Register the room in SYNC_INFO and update pending invite status atomically
                // This ensures the contract ID is associated with the owner_vk
                // when the response comes back, and prevents re-processing on retry
                info!(
                    "Registering room in SYNC_INFO for owner: {:?}, contract ID: {}",
                    MemberId::from(owner_vk),
                    contract_key.id()
                );

                // Use with_mut to scope the borrow properly and avoid AlreadyBorrowed errors
                SYNC_INFO.with_mut(|sync_info| {
                    sync_info.register_new_room(owner_vk);
                    sync_info.update_sync_status(&owner_vk, RoomSyncStatus::Subscribing);
                });

                // Update pending invite status to prevent re-processing on concurrent calls
                PENDING_INVITES.with_mut(|pending| {
                    if let Some(join) = pending.map.get_mut(&owner_vk) {
                        join.status = PendingRoomStatus::Subscribing;
                        join.subscribing_since = Some(now_ms());
                    }
                });

                // Create a get request without subscription (will subscribe after response)
                let get_request = ContractRequest::Get {
                    key: *contract_key.id(),    // GET uses ContractInstanceId
                    return_contract_code: true, // I think this should be false but apparently that was triggering a bug
                    subscribe: false,
                    blocking_subscribe: false,
                };

                let client_request = ClientRequest::ContractOp(get_request);

                // WebAPI availability was checked at the start of this function
                if let Some(web_api) = WEB_API.write().as_mut() {
                    match web_api.send(client_request).await {
                        Ok(_) => {
                            info!("Sent GetRequest for room {:?}", MemberId::from(owner_vk));
                        }
                        Err(e) => {
                            error!(
                                "Error sending GetRequest to room {:?}: {}",
                                MemberId::from(owner_vk),
                                e
                            );
                            // Update pending invite status to error
                            PENDING_INVITES.with_mut(|pending| {
                                if let Some(join) = pending.map.get_mut(&owner_vk) {
                                    join.status = PendingRoomStatus::Error(e.to_string());
                                }
                            });
                        }
                    }
                } else {
                    // This shouldn't happen since we checked at the start, but handle gracefully
                    warn!("WebAPI became unavailable during processing, resetting status");
                    PENDING_INVITES.with_mut(|pending| {
                        if let Some(join) = pending.map.get_mut(&owner_vk) {
                            join.status = PendingRoomStatus::PendingSubscription;
                        }
                    });
                }
            }
        }

        info!("Checking for rooms that need to be subscribed");

        // Only check rooms_awaiting_subscription if WebAPI is available
        let rooms_to_subscribe = if web_api_available {
            SYNC_INFO.with_mut(|sync_info| sync_info.rooms_awaiting_subscription())
        } else {
            std::collections::HashMap::new()
        };

        if !rooms_to_subscribe.is_empty() {
            for (owner_vk, state) in &rooms_to_subscribe {
                info!("Subscribing to room: {:?}", MemberId::from(*owner_vk));

                let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
                let parameters = ChatRoomParametersV1 { owner: *owner_vk };
                let params_bytes = to_cbor_vec(&parameters);
                let parameters = Parameters::from(params_bytes);

                let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
                    WrappedContract::new(Arc::new(contract_code), parameters),
                ));

                let wrapped_state = WrappedState::new(to_cbor_vec(state));

                // Create a put request without subscription (will subscribe after response)
                let contract_key = owner_vk_to_contract_key(owner_vk);
                let contract_id = contract_key.id();
                info!(
                    "Preparing PutRequest for room {:?} with contract ID: {}",
                    MemberId::from(*owner_vk),
                    contract_id
                );

                let put_request = ContractRequest::Put {
                    contract: contract_container,
                    state: wrapped_state,
                    related_contracts: Default::default(),
                    subscribe: true,
                    blocking_subscribe: false,
                };

                let client_request = ClientRequest::ContractOp(put_request);

                info!(
                    "Sending PutRequest for room {:?} with contract ID: {}",
                    MemberId::from(*owner_vk),
                    contract_id
                );

                if let Some(web_api) = WEB_API.write().as_mut() {
                    match web_api.send(client_request).await {
                        Ok(_) => {
                            info!("Sent PutRequest for room {:?}", MemberId::from(*owner_vk));
                            // Update the sync status to subscribing using with_mut
                            SYNC_INFO.with_mut(|sync_info| {
                                sync_info.update_sync_status(owner_vk, RoomSyncStatus::Subscribing);
                            });
                        }
                        Err(e) => {
                            // Don't fail the entire process if one room fails
                            error!(
                                "Error sending PutRequest to room {:?}: {}",
                                MemberId::from(*owner_vk),
                                e
                            );
                            // Update sync status to error using with_mut
                            SYNC_INFO.with_mut(|sync_info| {
                                sync_info.update_sync_status(
                                    owner_vk,
                                    RoomSyncStatus::Error(e.to_string()),
                                );
                            });
                        }
                    }
                } else {
                    // This shouldn't happen since we checked at the start
                    warn!("WebAPI became unavailable during processing");
                }
            }
        }

        // Send upgrade pointers for migrated rooms (owner only)
        if web_api_available {
            let migrated_rooms: Vec<(VerifyingKey, freenet_stdlib::prelude::ContractKey)> =
                ROOMS.with_mut(|rooms| std::mem::take(&mut rooms.migrated_rooms));

            for (owner_vk, old_contract_key) in &migrated_rooms {
                // Only the room owner should send the upgrade pointer
                let is_owner = ROOMS
                    .read()
                    .map
                    .get(owner_vk)
                    .is_some_and(|rd| rd.self_sk.verifying_key() == *owner_vk);

                if !is_owner {
                    continue;
                }

                info!(
                    "Sending upgrade pointer for migrated room {:?} from old contract {} to new contract",
                    MemberId::from(*owner_vk),
                    old_contract_key.id()
                );

                // Build the upgrade state
                let (upgrade_state, _new_contract_key) = {
                    let rooms = ROOMS.read();
                    if let Some(room_data) = rooms.map.get(owner_vk) {
                        use river_core::room_state::upgrade::{
                            AuthorizedUpgradeV1, OptionalUpgradeV1, UpgradeV1,
                        };

                        let new_contract_id = room_data.contract_key.id();
                        let mut id_bytes = [0u8; 32];
                        id_bytes.copy_from_slice(new_contract_id.as_bytes());
                        let new_address = blake3::Hash::from(id_bytes);
                        let upgrade = UpgradeV1 {
                            owner_member_id: room_data.owner_id(),
                            version: 1,
                            new_chatroom_address: new_address,
                        };
                        let authorized_upgrade =
                            AuthorizedUpgradeV1::new(upgrade, &room_data.self_sk);

                        // Create a minimal state with just the upgrade field set
                        let upgrade_state = ChatRoomStateV1 {
                            upgrade: OptionalUpgradeV1(Some(authorized_upgrade)),
                            ..Default::default()
                        };

                        (upgrade_state, room_data.contract_key)
                    } else {
                        continue;
                    }
                };

                let update_request = ContractRequest::Update {
                    key: *old_contract_key,
                    data: UpdateData::State(to_cbor_vec(&upgrade_state).into()),
                };

                let client_request = ClientRequest::ContractOp(update_request);

                if let Some(web_api) = WEB_API.write().as_mut() {
                    match web_api.send(client_request).await {
                        Ok(_) => {
                            info!(
                                "Sent upgrade pointer for room {:?} to old contract {}",
                                MemberId::from(*owner_vk),
                                old_contract_key.id()
                            );
                        }
                        Err(e) => {
                            warn!(
                                "Failed to send upgrade pointer for room {:?}: {}",
                                MemberId::from(*owner_vk),
                                e
                            );
                        }
                    }
                }
            }
        }

        info!("Checking for rooms to update");

        // Only check for rooms needing updates if WebAPI is available
        let rooms_to_sync = if web_api_available {
            SYNC_INFO.with_mut(|sync_info| sync_info.needs_to_send_update())
        } else {
            std::collections::HashMap::new()
        };

        info!(
            "Found {} rooms that need synchronization",
            rooms_to_sync.len()
        );

        for (room_vk, state) in &rooms_to_sync {
            info!("Processing room: {:?}", MemberId::from(*room_vk));

            let contract_key = owner_vk_to_contract_key(room_vk);

            let update_request = ContractRequest::Update {
                key: contract_key,
                data: UpdateData::State(to_cbor_vec(state).into()),
            };

            let client_request = ClientRequest::ContractOp(update_request);

            if let Some(web_api) = WEB_API.write().as_mut() {
                match web_api.send(client_request).await {
                    Ok(_) => {
                        info!(
                            "Successfully sent update for room: {:?}",
                            MemberId::from(*room_vk)
                        );
                        // Only update the last synced state after successfully sending the update
                        SYNC_INFO.with_mut(|sync_info| {
                            sync_info.state_updated(room_vk, state.clone());
                        });
                    }
                    Err(e) => {
                        // Don't fail the entire process if one room fails
                        error!(
                            "Failed to send update for room {:?}: {}",
                            MemberId::from(*room_vk),
                            e
                        );
                    }
                }
            } else {
                // This shouldn't happen since we checked at the start
                warn!("WebAPI became unavailable during processing");
            }
        }

        info!("Finished processing all rooms");

        Ok(())
    }

    /// Updates the room state and last_sync_state, should be called after state update received from network
    pub(crate) fn update_room_state(&self, room_owner_vk: &VerifyingKey, state: &ChatRoomStateV1) {
        // Capture data needed for notifications BEFORE the mutable borrow
        let (old_message_ids, self_member_id, member_info_clone, room_secrets) = {
            let rooms = ROOMS.read();
            if let Some(room_data) = rooms.map.get(room_owner_vk) {
                let old_ids: std::collections::HashSet<_> = room_data
                    .room_state
                    .recent_messages
                    .messages
                    .iter()
                    .map(|m| m.id())
                    .collect();
                info!(
                    "update_room_state: Captured {} old message IDs for room {:?}",
                    old_ids.len(),
                    MemberId::from(*room_owner_vk)
                );
                let self_id = MemberId::from(&room_data.self_sk.verifying_key());
                let member_info = room_data.room_state.member_info.clone();
                let secrets = room_data.secrets.clone();
                (Some(old_ids), Some(self_id), Some(member_info), secrets)
            } else {
                info!(
                    "update_room_state: Room {:?} not found in ROOMS when capturing old IDs",
                    MemberId::from(*room_owner_vk)
                );
                (None, None, None, HashMap::new())
            }
        };

        // Log incoming state message count
        info!(
            "update_room_state: Incoming state has {} messages for room {:?}",
            state.recent_messages.messages.len(),
            MemberId::from(*room_owner_vk)
        );

        // Will be populated inside with_mut if new messages are detected
        let mut pending_notification: Option<(Vec<_>, MemberId)> = None;
        let room_owner_copy = *room_owner_vk;

        ROOMS.with_mut(|rooms| {
            if let Some(room_data) = rooms.map.get_mut(room_owner_vk) {
                // Log member info versions before merge
                info!(
                    "Before merge - Local member info versions ({} items):",
                    room_data.room_state.member_info.member_info.len()
                );
                for info in &room_data.room_state.member_info.member_info {
                    info!(
                        "  Member: {:?}, Version: {}, Nickname: {}",
                        info.member_info.member_id,
                        info.member_info.version,
                        info.member_info.preferred_nickname
                    );
                }

                info!(
                    "Before merge - Incoming state member info versions ({} items):",
                    state.member_info.member_info.len()
                );
                for info in &state.member_info.member_info {
                    info!(
                        "  Member: {:?}, Version: {}, Nickname: {}",
                        info.member_info.member_id,
                        info.member_info.version,
                        info.member_info.preferred_nickname
                    );
                }

                // Update the room state by merging the new state with the existing one
                match room_data.room_state.merge(
                    &room_data.room_state.clone(),
                    &ChatRoomParametersV1 {
                        owner: *room_owner_vk,
                    },
                    state,
                ) {
                    Ok(_) => {
                        // For private rooms, rebuild actions_state with decrypted content
                        let is_private = room_data.room_state.configuration.configuration.privacy_mode
                            == PrivacyMode::Private;
                        if is_private {
                            // Decrypt all private action messages using version-aware lookup
                            let decrypted_actions: HashMap<MessageId, Vec<u8>> = room_data
                                .room_state
                                .recent_messages
                                .messages
                                .iter()
                                .filter(|msg| msg.message.content.is_action())
                                .filter_map(|msg| {
                                    if let RoomMessageBody::Private { ciphertext, nonce, secret_version, .. } =
                                        &msg.message.content
                                    {
                                        room_data.get_secret_for_version(*secret_version)
                                            .and_then(|secret| {
                                                decrypt_with_symmetric_key(secret, ciphertext, nonce)
                                                    .ok()
                                                    .map(|plaintext| (msg.id(), plaintext))
                                            })
                                    } else {
                                        None
                                    }
                                })
                                .collect();

                            room_data
                                .room_state
                                .recent_messages
                                .rebuild_actions_state_with_decrypted(&decrypted_actions);
                        }

                        // Log member info versions after merge
                        info!(
                            "After merge - Updated member info versions ({} items):",
                            room_data.room_state.member_info.member_info.len()
                        );
                        for info in &room_data.room_state.member_info.member_info {
                            info!(
                                "  Member: {:?}, Version: {}, Nickname: {}",
                                info.member_info.member_id,
                                info.member_info.version,
                                info.member_info.preferred_nickname
                            );
                        }

                        // Make sure the room is registered in SYNC_INFO
                        SYNC_INFO.with_mut(|sync_info| {
                            sync_info.register_new_room(*room_owner_vk);
                            // We use the post-merged state to avoid some edge cases
                            sync_info
                                .update_last_synced_state(room_owner_vk, &room_data.room_state);
                        });

                        // Mark initial sync complete for this room (enables notifications)
                        mark_initial_sync_complete(room_owner_vk);

                        // Detect new messages - store for notification AFTER with_mut completes
                        // (notify_new_messages calls ROOMS.read() internally, causing deadlock if called here)
                        if let (Some(old_ids), Some(self_id), Some(_member_info)) =
                            (&old_message_ids, self_member_id, &member_info_clone)
                        {
                            let new_messages: Vec<_> = room_data
                                .room_state
                                .recent_messages
                                .messages
                                .iter()
                                .filter(|m| !old_ids.contains(&m.id()))
                                .cloned()
                                .collect();

                            if !new_messages.is_empty() {
                                info!(
                                    "Detected {} new messages in state update for room {:?}",
                                    new_messages.len(),
                                    MemberId::from(*room_owner_vk)
                                );

                                // Note: we do NOT record receive times here. Full state
                                // updates don't reflect real-time arrival — we don't know
                                // when these messages actually propagated to our node.
                                // Only apply_delta (subscription deltas) captures the
                                // true arrival moment.

                                // Store for notification after with_mut completes
                                pending_notification = Some((new_messages, self_id));
                            } else {
                                info!(
                                    "No new messages detected for room {:?} (old_ids: {}, post-merge: {})",
                                    MemberId::from(*room_owner_vk),
                                    old_ids.len(),
                                    room_data.room_state.recent_messages.messages.len()
                                );
                            }
                        }

                        // Persist to delegate so state survives refresh
                        wasm_bindgen_futures::spawn_local(async {
                            if let Err(e) = save_rooms_to_delegate().await {
                                error!("Failed to save rooms to delegate after state update: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        error!("Failed to merge room state: {}", e);
                    }
                }
            } else {
                warn!("Room not found in rooms map for update_room_state. This can happen if we receive an update before the room is fully initialized.");
                // We cannot create a room here because we don't have the self_sk (signing key)
                // Instead, we should request the full state with a GET reques
                // This is handled by registering the room in SYNC_INFO which will trigger a GET request in the next sync cycle

                // Register the room in SYNC_INFO to trigger a GET reques
                SYNC_INFO.with_mut(|sync_info| {
                    sync_info.register_new_room(*room_owner_vk);
                    // Store the state temporarily so it can be merged when we get the full room data
                    sync_info.update_last_synced_state(room_owner_vk, state);
                });

                info!("Registered room {:?} for GET request after receiving update without existing room data", MemberId::from(*room_owner_vk));
            }
        });

        // Update document title after ROOMS.with_mut completes (update_document_title calls ROOMS.read())
        update_document_title();

        // Now safe to call notify_new_messages (it calls ROOMS.read() internally)
        if let (Some((new_messages, self_id)), Some(member_info)) =
            (pending_notification, member_info_clone)
        {
            notify_new_messages(
                &room_owner_copy,
                &new_messages,
                self_id,
                &member_info,
                &room_secrets,
            );

            // If user is viewing this room with tab visible, mark as read
            let is_visible = *DOCUMENT_VISIBLE.read();
            let is_current_room = CURRENT_ROOM.read().owner_key == Some(room_owner_copy);
            if is_visible && is_current_room {
                mark_current_room_as_read();
            }
        }
    }

    /// Refresh all room states by sending GET requests.
    /// This is used after PC suspension/wake to catch any updates that were missed
    /// while the page was hidden or the machine was suspended.
    pub async fn refresh_all_rooms(&self) -> Result<(), SynchronizerError> {
        info!("Refreshing all rooms to catch missed updates");

        // Check if WebAPI is available
        let web_api_available = WEB_API.read().is_some();
        if !web_api_available {
            warn!("WebAPI not available, skipping room refresh");
            return Err(SynchronizerError::ApiNotInitialized);
        }

        // Collect all room owner keys that we're currently tracking
        let room_owners: Vec<VerifyingKey> = ROOMS.read().map.keys().copied().collect();

        if room_owners.is_empty() {
            info!("No rooms to refresh");
            return Ok(());
        }

        info!("Refreshing {} rooms", room_owners.len());

        for owner_vk in room_owners {
            let contract_key = owner_vk_to_contract_key(&owner_vk);

            // Send a GET request to fetch the current state
            // This will trigger a response that merges any missed updates
            let get_request = ContractRequest::Get {
                key: *contract_key.id(),
                return_contract_code: false,
                subscribe: false, // Already subscribed, just need the state
                blocking_subscribe: false,
            };

            let client_request = ClientRequest::ContractOp(get_request);

            if let Some(web_api) = WEB_API.write().as_mut() {
                match web_api.send(client_request).await {
                    Ok(_) => {
                        info!(
                            "Sent refresh GET request for room {:?}",
                            MemberId::from(owner_vk)
                        );
                    }
                    Err(e) => {
                        // Don't fail the entire refresh if one room fails
                        error!(
                            "Error sending refresh GET for room {:?}: {}",
                            MemberId::from(owner_vk),
                            e
                        );
                    }
                }
            } else {
                warn!("WebAPI became unavailable during refresh");
                return Err(SynchronizerError::ApiNotInitialized);
            }
        }

        info!("Finished sending refresh requests for all rooms");
        Ok(())
    }

    /// Fetch the current state of a contract via GET request.
    /// Used after successful subscribe to ensure we have the latest state,
    /// since delegate storage may contain stale data from a previous session.
    pub async fn get_contract_state(
        &self,
        contract_key: &ContractKey,
    ) -> Result<(), SynchronizerError> {
        info!("Fetching current state for contract: {}", contract_key.id());

        let get_request = ContractRequest::Get {
            key: *contract_key.id(),
            return_contract_code: false,
            subscribe: false,
            blocking_subscribe: false,
        };

        let client_request = ClientRequest::ContractOp(get_request);

        if let Some(web_api) = WEB_API.write().as_mut() {
            match web_api.send(client_request).await {
                Ok(_) => {
                    info!("Sent GET request for contract: {}", contract_key.id());
                    Ok(())
                }
                Err(e) => {
                    error!("Failed to send GET request for contract: {}", e);
                    Err(SynchronizerError::ClientApiError(e.to_string()))
                }
            }
        } else {
            warn!("WebAPI not available for GET request");
            Err(SynchronizerError::ApiNotInitialized)
        }
    }

    /// Subscribe to a contract after a successful GET or PUT operation
    pub async fn subscribe_to_contract(
        &self,
        contract_key: &ContractKey,
    ) -> Result<(), SynchronizerError> {
        info!("Subscribing to contract with key: {}", contract_key.id());

        let subscribe_request = ContractRequest::Subscribe {
            key: *contract_key.id(), // Subscribe uses ContractInstanceId
            summary: None,
        };

        let client_request = ClientRequest::ContractOp(subscribe_request);

        if let Some(web_api) = WEB_API.write().as_mut() {
            match web_api.send(client_request).await {
                Ok(_) => {
                    info!(
                        "Successfully sent subscription request for contract: {}",
                        contract_key.id()
                    );
                    Ok(())
                }
                Err(e) => {
                    error!("Failed to send subscription request: {}", e);
                    Err(SynchronizerError::SubscribeError(e.to_string()))
                }
            }
        } else {
            warn!("WebAPI not available, skipping subscription");
            Err(SynchronizerError::ApiNotInitialized)
        }
    }
}

/// Stores information about a contract being synchronized
#[derive(Clone)]
pub struct ContractSyncInfo {
    pub owner_vk: VerifyingKey,
}
