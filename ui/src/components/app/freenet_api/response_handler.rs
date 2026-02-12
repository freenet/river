mod get_response;
mod put_response;
mod subscribe_response;
mod update_notification;
mod update_response;

use super::error::SynchronizerError;
use super::room_synchronizer::RoomSynchronizer;
use crate::components::app::chat_delegate::{
    complete_pending_public_key_request, complete_pending_request, complete_pending_sign_request,
    complete_pending_signing_key_request, is_legacy_delegate_key, mark_legacy_migration_done,
    save_rooms_to_delegate, ROOMS_STORAGE_KEY,
};
use crate::components::app::document_title::{mark_current_room_as_read, update_document_title};
use crate::components::app::notifications::mark_initial_sync_complete;
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::room_data::CurrentRoom;
use crate::room_data::Rooms;
use crate::util::ecies::{decrypt_secret_from_member_blob, decrypt_with_symmetric_key};
use crate::util::owner_vk_to_contract_key;
use ciborium::de::from_reader;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::ReadableExt;
use freenet_stdlib::client_api::{ContractResponse, HostResponse};
use freenet_stdlib::prelude::OutboundDelegateMsg;
pub use get_response::handle_get_response;
pub use put_response::handle_put_response;
use river_core::chat_delegate::{ChatDelegateRequestMsg, ChatDelegateResponseMsg};
use river_core::room_state::member::MemberId;
use river_core::room_state::message::{MessageId, RoomMessageBody};
use river_core::room_state::privacy::PrivacyMode;
use std::collections::HashMap;
pub use subscribe_response::handle_subscribe_response;
pub use update_notification::handle_update_notification;
pub use update_response::handle_update_response;
use x25519_dalek::PublicKey as X25519PublicKey;

/// Handles responses from the Freenet API
pub struct ResponseHandler {
    room_synchronizer: RoomSynchronizer,
}

/// Response flags returned from handle_api_response
#[derive(Default)]
pub struct ResponseFlags {
    /// True if a re-PUT should be scheduled (subscription failed but we have local state)
    pub needs_reput: bool,
    /// True if subscriptions were initiated and need timeout monitoring
    pub subscriptions_initiated: bool,
}

impl ResponseHandler {
    pub fn new(room_synchronizer: RoomSynchronizer) -> Self {
        Self { room_synchronizer }
    }

    // Create a new ResponseHandler that shares the same RoomSynchronizer
    pub fn new_with_shared_synchronizer(synchronizer: &RoomSynchronizer) -> Self {
        // Clone the RoomSynchronizer to share the same state
        Self {
            room_synchronizer: synchronizer.clone(),
        }
    }

    /// Handles individual API responses.
    /// Returns flags indicating what follow-up actions are needed.
    pub async fn handle_api_response(
        &mut self,
        response: HostResponse,
    ) -> Result<ResponseFlags, SynchronizerError> {
        let mut flags = ResponseFlags::default();

        match response {
            HostResponse::Ok => {
                info!("Received OK response from API");
            }
            HostResponse::ContractResponse(contract_response) => match contract_response {
                ContractResponse::GetResponse {
                    key,
                    contract: _,
                    state,
                } => {
                    handle_get_response(
                        &mut self.room_synchronizer,
                        key,
                        Vec::new(),
                        state.to_vec(),
                    )
                    .await?;
                }
                ContractResponse::PutResponse { key } => {
                    handle_put_response(&mut self.room_synchronizer, key).await?;
                }
                ContractResponse::UpdateNotification { key, update } => {
                    handle_update_notification(&mut self.room_synchronizer, key, update)?;
                }
                ContractResponse::UpdateResponse { key, summary } => {
                    handle_update_response(key, summary.to_vec());
                }
                ContractResponse::SubscribeResponse { key, subscribed } => {
                    flags.needs_reput = handle_subscribe_response(key, subscribed);
                }
                _ => {
                    info!("Unhandled contract response: {:?}", contract_response);
                }
            },
            HostResponse::DelegateResponse { key, values } => {
                // Check if this is a response from any known legacy delegate
                let is_legacy_delegate = is_legacy_delegate_key(key.bytes());
                if is_legacy_delegate {
                    info!("Received response from LEGACY delegate - checking for migration data");
                }

                info!(
                    "Received delegate response from API with key: {:?} containing {} values",
                    key,
                    values.len()
                );
                for (i, v) in values.iter().enumerate() {
                    info!("Processing delegate response value #{}", i);
                    match v {
                        OutboundDelegateMsg::ApplicationMessage(app_msg) => {
                            info!(
                                "Delegate response is an ApplicationMessage, processed flag: {}",
                                app_msg.processed
                            );

                            // Log the raw payload for debugging
                            let payload_str = if app_msg.payload.len() < 100 {
                                format!("{:?}", app_msg.payload)
                            } else {
                                format!("{:?}... (truncated)", &app_msg.payload[..100])
                            };
                            info!("ApplicationMessage payload: {}", payload_str);

                            // Try to deserialize as a response
                            let deserialization_result = from_reader::<ChatDelegateResponseMsg, _>(
                                app_msg.payload.as_slice(),
                            );

                            // Also try to deserialize as a request to see if that's what's happening
                            let request_deser_result = from_reader::<ChatDelegateRequestMsg, _>(
                                app_msg.payload.as_slice(),
                            );
                            info!(
                                "Deserialization as request result: {:?}",
                                request_deser_result.is_ok()
                            );

                            if let Ok(response) = deserialization_result {
                                info!(
                                    "Successfully deserialized as ChatDelegateResponseMsg: {:?}",
                                    response
                                );

                                // Try to complete any pending request waiting for this response
                                let completed = match &response {
                                    // Key-value storage responses
                                    ChatDelegateResponseMsg::GetResponse { key, .. } => {
                                        complete_pending_request(key, response.clone())
                                    }
                                    ChatDelegateResponseMsg::StoreResponse { key, .. } => {
                                        complete_pending_request(key, response.clone())
                                    }
                                    ChatDelegateResponseMsg::DeleteResponse { key, .. } => {
                                        complete_pending_request(key, response.clone())
                                    }
                                    ChatDelegateResponseMsg::ListResponse { .. } => {
                                        // Use the special list request key
                                        let list_key =
                                            river_core::chat_delegate::ChatDelegateKey::new(
                                                b"__list_request__".to_vec(),
                                            );
                                        complete_pending_request(&list_key, response.clone())
                                    }
                                    // Signing key management responses
                                    ChatDelegateResponseMsg::StoreSigningKeyResponse {
                                        room_key,
                                        ..
                                    } => complete_pending_signing_key_request(
                                        room_key,
                                        response.clone(),
                                    ),
                                    ChatDelegateResponseMsg::GetPublicKeyResponse {
                                        room_key,
                                        ..
                                    } => complete_pending_public_key_request(
                                        room_key,
                                        response.clone(),
                                    ),
                                    // Signing response - use both room_key and request_id for correlation
                                    ChatDelegateResponseMsg::SignResponse {
                                        room_key,
                                        request_id,
                                        ..
                                    } => complete_pending_sign_request(
                                        room_key,
                                        *request_id,
                                        response.clone(),
                                    ),
                                };

                                if completed {
                                    info!("Completed pending delegate request");
                                }

                                // Process the response based on its type
                                match response {
                                    ChatDelegateResponseMsg::GetResponse { key, value } => {
                                        info!(
                                            "Got value for key: {:?}, value present: {}",
                                            String::from_utf8_lossy(key.as_bytes()),
                                            value.is_some()
                                        );

                                        // Check if this is the rooms data
                                        if key.as_bytes() == ROOMS_STORAGE_KEY {
                                            if let Some(rooms_data) = value {
                                                // Deserialize the rooms data
                                                match from_reader::<Rooms, _>(&rooms_data[..]) {
                                                    Ok(loaded_rooms) => {
                                                        // TODO: Remove legacy migration code after 2026-03-01
                                                        if is_legacy_delegate {
                                                            info!("Successfully loaded rooms from LEGACY delegate - migrating to new delegate");
                                                        } else {
                                                            info!("Successfully loaded rooms from delegate");
                                                        }

                                                        // Restore the current room selection if saved
                                                        if let Some(saved_room_key) =
                                                            loaded_rooms.current_room_key
                                                        {
                                                            info!("Restoring current room selection from delegate");
                                                            *CURRENT_ROOM.write() = CurrentRoom {
                                                                owner_key: Some(saved_room_key),
                                                            };
                                                        }

                                                        // Collect room keys before merge
                                                        let room_keys: Vec<_> = loaded_rooms
                                                            .map
                                                            .keys()
                                                            .copied()
                                                            .collect();

                                                        // Merge the loaded rooms with the current rooms
                                                        ROOMS.with_mut(|current_rooms| {
                                                            if let Err(e) = current_rooms.merge(loaded_rooms) {
                                                                error!("Failed to merge rooms: {}", e);
                                                            } else {
                                                                info!("Successfully merged rooms from delegate");

                                                                // Re-decrypt ALL secret versions for each room (secrets are #[serde(skip)])
                                                                for room_data in current_rooms.map.values_mut() {
                                                                    if room_data.room_state.configuration.configuration.privacy_mode == PrivacyMode::Private {
                                                                        let member_id = MemberId::from(&room_data.self_sk.verifying_key());
                                                                        let current_version = room_data.room_state.secrets.current_version;
                                                                        let self_sk = room_data.self_sk.clone();

                                                                        // Extract encrypted secret data to avoid borrow issues
                                                                        let member_secrets: Vec<_> = room_data
                                                                            .room_state
                                                                            .secrets
                                                                            .encrypted_secrets
                                                                            .iter()
                                                                            .filter(|s| s.secret.member_id == member_id)
                                                                            .map(|s| (
                                                                                s.secret.secret_version,
                                                                                s.secret.ciphertext.clone(),
                                                                                s.secret.nonce,
                                                                                s.secret.sender_ephemeral_public_key,
                                                                            ))
                                                                            .collect();

                                                                        if member_secrets.is_empty() {
                                                                            warn!("No encrypted secrets found for member {:?}", member_id);
                                                                        } else {
                                                                            info!("Found {} encrypted secrets for member {:?}", member_secrets.len(), member_id);
                                                                            for (version, ciphertext, nonce, ephemeral_key_bytes) in member_secrets {
                                                                                let ephemeral_key = X25519PublicKey::from(ephemeral_key_bytes);
                                                                                match decrypt_secret_from_member_blob(
                                                                                    &ciphertext,
                                                                                    &nonce,
                                                                                    &ephemeral_key,
                                                                                    &self_sk,
                                                                                ) {
                                                                                    Ok(decrypted_secret) => {
                                                                                        info!("Re-decrypted room secret version {} for member {:?}", version, member_id);
                                                                                        room_data.set_secret(decrypted_secret, version);
                                                                                    }
                                                                                    Err(e) => {
                                                                                        warn!("Failed to re-decrypt room secret version {}: {}", version, e);
                                                                                    }
                                                                                }
                                                                            }
                                                                        }

                                                                        // Ensure current_secret_version is set to the actual current version
                                                                        room_data.current_secret_version = Some(current_version);
                                                                    }
                                                                }

                                                                // Rebuild actions_state for each loaded room
                                                                // This is needed because actions_state is #[serde(skip)] and not serialized
                                                                for room_data in current_rooms.map.values_mut() {
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
                                                                                    // Look up the secret for this message's version
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
                                                                    } else {
                                                                        // Public room - rebuild from public action messages
                                                                        room_data
                                                                            .room_state
                                                                            .recent_messages
                                                                            .rebuild_actions_state();
                                                                    }
                                                                }
                                                            }
                                                        });

                                                        // Mark current room as read since user is viewing it
                                                        // (must be after merge so room data exists)
                                                        mark_current_room_as_read();
                                                        update_document_title();

                                                        // Migrate signing keys to delegate for each loaded room
                                                        info!("Migrating signing keys to delegate for {} rooms", room_keys.len());
                                                        for room_key in &room_keys {
                                                            // Get the room's signing key
                                                            let signing_key_opt =
                                                                ROOMS.with(|rooms| {
                                                                    rooms.map.get(room_key).map(
                                                                        |room_data| {
                                                                            (
                                                                                room_data
                                                                                    .room_key(),
                                                                                room_data
                                                                                    .self_sk
                                                                                    .clone(),
                                                                            )
                                                                        },
                                                                    )
                                                                });

                                                            if let Some((
                                                                delegate_room_key,
                                                                signing_key,
                                                            )) = signing_key_opt
                                                            {
                                                                // Spawn async migration task
                                                                let room_key_copy = *room_key;
                                                                wasm_bindgen_futures::spawn_local(
                                                                    async move {
                                                                        let migrated = crate::signing::migrate_signing_key(
                                                                        delegate_room_key,
                                                                        &signing_key,
                                                                    )
                                                                    .await;

                                                                        if migrated {
                                                                            // Mark the room as migrated
                                                                            ROOMS.with_mut(|rooms| {
                                                                            if let Some(room_data) = rooms.map.get_mut(&room_key_copy) {
                                                                                room_data.key_migrated_to_delegate = true;
                                                                            }
                                                                        });
                                                                        }
                                                                    },
                                                                );
                                                            }
                                                        }

                                                        // Mark all loaded rooms as having completed initial sync
                                                        // and subscribe to receive updates
                                                        for room_key in &room_keys {
                                                            mark_initial_sync_complete(room_key);
                                                        }

                                                        // Subscribe to each loaded room's contract
                                                        info!(
                                                            "Subscribing to {} rooms loaded from delegate",
                                                            room_keys.len()
                                                        );
                                                        for room_key in room_keys {
                                                            // Register the room in SYNC_INFO
                                                            SYNC_INFO
                                                                .write()
                                                                .register_new_room(room_key);
                                                            SYNC_INFO.write().update_sync_status(
                                                                &room_key,
                                                                RoomSyncStatus::Subscribing,
                                                            );

                                                            // Get contract key and subscribe
                                                            let contract_key =
                                                                owner_vk_to_contract_key(&room_key);
                                                            if let Err(e) = self
                                                                .room_synchronizer
                                                                .subscribe_to_contract(
                                                                    &contract_key,
                                                                )
                                                                .await
                                                            {
                                                                error!(
                                                                    "Failed to subscribe to loaded room {:?}: {}",
                                                                    contract_key.id(),
                                                                    e
                                                                );
                                                            } else {
                                                                info!(
                                                                    "Successfully sent subscribe request for loaded room {:?}",
                                                                    contract_key.id()
                                                                );
                                                                // Mark that subscriptions were initiated so timeout monitoring can be scheduled
                                                                flags.subscriptions_initiated =
                                                                    true;
                                                            }
                                                        }

                                                        // TODO: Remove legacy migration code after 2026-03-01
                                                        // If this was from the legacy delegate, save to the new delegate
                                                        if is_legacy_delegate {
                                                            info!("Migrating room data from legacy delegate to new delegate");
                                                            wasm_bindgen_futures::spawn_local(
                                                                async {
                                                                    match save_rooms_to_delegate()
                                                                        .await
                                                                    {
                                                                        Ok(_) => {
                                                                            info!("Successfully migrated room data to new delegate");
                                                                            mark_legacy_migration_done();
                                                                        }
                                                                        Err(e) => {
                                                                            error!("Failed to migrate room data to new delegate: {}", e);
                                                                            // Don't mark as done - will retry on next startup
                                                                        }
                                                                    }
                                                                },
                                                            );
                                                        }
                                                    }
                                                    Err(e) => {
                                                        error!(
                                                            "Failed to deserialize rooms data: {}",
                                                            e
                                                        );
                                                    }
                                                }
                                            } else {
                                                info!("No rooms data found in delegate");
                                                // TODO: Remove legacy migration code after 2026-03-01
                                                // If legacy delegate has no data, mark migration done so we don't keep trying
                                                if is_legacy_delegate {
                                                    info!("No rooms in legacy delegate - marking migration complete");
                                                    mark_legacy_migration_done();
                                                }
                                            }
                                        } else {
                                            warn!(
                                                "Unexpected key in GetResponse: {:?}",
                                                String::from_utf8_lossy(key.as_bytes())
                                            );
                                        }
                                    }
                                    ChatDelegateResponseMsg::ListResponse { keys } => {
                                        info!("Listed {} keys", keys.len());
                                    }
                                    ChatDelegateResponseMsg::StoreResponse {
                                        key,
                                        result,
                                        value_size: _,
                                    } => match result {
                                        Ok(_) => info!(
                                            "Successfully stored key: {:?}",
                                            String::from_utf8_lossy(key.as_bytes())
                                        ),
                                        Err(e) => warn!(
                                            "Failed to store key: {:?}, error: {}",
                                            String::from_utf8_lossy(key.as_bytes()),
                                            e
                                        ),
                                    },
                                    ChatDelegateResponseMsg::DeleteResponse { key, result } => {
                                        match result {
                                            Ok(_) => info!(
                                                "Successfully deleted key: {:?}",
                                                String::from_utf8_lossy(key.as_bytes())
                                            ),
                                            Err(e) => warn!(
                                                "Failed to delete key: {:?}, error: {}",
                                                String::from_utf8_lossy(key.as_bytes()),
                                                e
                                            ),
                                        }
                                    }
                                    // Signing key management responses
                                    ChatDelegateResponseMsg::StoreSigningKeyResponse {
                                        room_key,
                                        result,
                                    } => match result {
                                        Ok(_) => {
                                            info!("Stored signing key for room: {:?}", room_key)
                                        }
                                        Err(e) => warn!("Failed to store signing key: {}", e),
                                    },
                                    ChatDelegateResponseMsg::GetPublicKeyResponse {
                                        room_key,
                                        public_key,
                                    } => {
                                        info!(
                                            "Got public key for room {:?}: present={}",
                                            room_key,
                                            public_key.is_some()
                                        );
                                    }
                                    ChatDelegateResponseMsg::SignResponse {
                                        room_key,
                                        signature,
                                        ..
                                    } => match signature {
                                        Ok(_) => info!("Got signature for room: {:?}", room_key),
                                        Err(e) => {
                                            warn!("Failed to sign for room {:?}: {}", room_key, e)
                                        }
                                    },
                                }
                            } else {
                                warn!("Failed to deserialize chat delegate response");
                            }
                        }
                        _ => {
                            warn!("Unhandled delegate response: {:?}", v);
                        }
                    }
                }
            }
            _ => {
                warn!("Unhandled API response: {:?}", response);
            }
        }
        Ok(flags)
    }

    pub fn get_room_synchronizer_mut(&mut self) -> &mut RoomSynchronizer {
        &mut self.room_synchronizer
    }

    // Get a reference to the room synchronizer
    pub fn get_room_synchronizer(&self) -> &RoomSynchronizer {
        &self.room_synchronizer
    }
}
