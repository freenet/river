use crate::components::app::freenet_api::error::SynchronizerError;
use crate::components::app::freenet_api::room_synchronizer::RoomSynchronizer;
use crate::components::app::notifications::mark_initial_sync_complete;
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{CURRENT_ROOM, PENDING_INVITES, ROOMS};
use crate::invites::PendingRoomStatus;
use crate::room_data::RoomData;
use crate::util::ecies::decrypt_secret_from_member_blob;
use crate::util::{from_cbor_slice, owner_vk_to_contract_key};
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::ReadableExt;
use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::ContractKey;
use river_core::room_state::member::{MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::privacy::{PrivacyMode, SealedBytes};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use x25519_dalek::PublicKey as X25519PublicKey;

pub async fn handle_get_response(
    room_synchronizer: &mut RoomSynchronizer,
    key: ContractKey,
    _contract: Vec<u8>,
    state: Vec<u8>,
) -> Result<(), SynchronizerError> {
    info!("Received get response for key {key}");

    // First try to find the owner_vk from SYNC_INFO
    let owner_vk = SYNC_INFO.read().get_owner_vk_for_instance_id(key.id());

    // If we couldn't find it in SYNC_INFO, try fallback mechanisms
    let owner_vk = if owner_vk.is_none() {
        // This is a fallback mechanism in case SYNC_INFO wasn't properly set up
        warn!(
            "Owner VK not found in SYNC_INFO for contract ID: {}, trying fallback",
            key.id()
        );

        // First try PENDING_INVITES
        let pending_invites = PENDING_INVITES.read();
        let mut found_owner_vk = None;

        for (owner_key, _) in pending_invites.map.iter() {
            let contract_key = owner_vk_to_contract_key(owner_key);
            if contract_key.id() == key.id() {
                info!(
                    "Found matching owner key in pending invites: {:?}",
                    MemberId::from(*owner_key)
                );
                found_owner_vk = Some(*owner_key);
                break;
            }
        }
        drop(pending_invites);

        // If not in pending invites, try ROOMS (for refresh after suspension)
        if found_owner_vk.is_none() {
            let rooms = ROOMS.read();
            for (owner_key, room_data) in rooms.map.iter() {
                if room_data.contract_key.id() == key.id() {
                    info!(
                        "Found matching owner key in existing rooms: {:?}",
                        MemberId::from(*owner_key)
                    );
                    found_owner_vk = Some(*owner_key);
                    break;
                }
            }
        }

        found_owner_vk
    } else {
        owner_vk
    };

    // Now check if this is for a pending invitation or an existing room needing refresh
    if let Some(owner_vk) = owner_vk {
        let is_pending_invite = PENDING_INVITES.read().map.contains_key(&owner_vk);
        let is_existing_room = ROOMS.read().map.contains_key(&owner_vk);

        if is_pending_invite {
            info!("This is a subscription for a pending invitation, adding state");
            let retrieved_state: ChatRoomStateV1 = from_cbor_slice::<ChatRoomStateV1>(&state);

            // Get the pending invite data once to avoid multiple reads
            let (self_sk, authorized_member, preferred_nickname) = {
                let pending_invites = PENDING_INVITES.read();
                let invite = &pending_invites.map[&owner_vk];
                (
                    invite.invitee_signing_key.clone(),
                    invite.authorized_member.clone(),
                    invite.preferred_nickname.clone(),
                )
            };

            // Prepare the member ID for checking
            let member_id: MemberId = authorized_member.member.member_vk.into();

            // Update the room data
            ROOMS.with_mut(|rooms| {
                // Get the entry for this room
                let entry = rooms.map.entry(owner_vk);

                // Check if this is a new entry before inserting
                let is_new_entry = matches!(entry, std::collections::hash_map::Entry::Vacant(_));

                // Insert or get the existing room data
                let room_data = entry.or_insert_with(|| {
                    // Create new room data if it doesn't exist
                    RoomData {
                        owner_vk,
                        room_state: retrieved_state.clone(),
                        self_sk: self_sk.clone(),
                        contract_key: key,
                        current_secret: None,
                        current_secret_version: None,
                        last_secret_rotation: None,
                        key_migrated_to_delegate: false, // Will be checked/migrated on startup
                    }
                });

                // If the room already existed, merge the retrieved state
                if !is_new_entry {
                    // Create parameters for merge
                    let params = ChatRoomParametersV1 { owner: owner_vk };

                    // Clone current state to avoid borrow issues during merge
                    let current_state = room_data.room_state.clone();

                    // Merge the retrieved state into the existing state
                    room_data
                        .room_state
                        .merge(&current_state, &params, &retrieved_state)
                        .expect("Failed to merge room states");
                }

                // Decrypt room secret if this is a private room
                if room_data.room_state.configuration.configuration.privacy_mode == PrivacyMode::Private {
                    let current_version = room_data.room_state.secrets.current_version;

                    // Find the encrypted secret for this member at the current version
                    if let Some(encrypted_secret) = room_data
                        .room_state
                        .secrets
                        .encrypted_secrets
                        .iter()
                        .find(|s| {
                            s.secret.member_id == member_id
                                && s.secret.secret_version == current_version
                        })
                    {
                        // Decrypt the secret using the member's signing key
                        let ephemeral_key_bytes = encrypted_secret.secret.sender_ephemeral_public_key;
                        let ephemeral_key = X25519PublicKey::from(ephemeral_key_bytes);

                        match decrypt_secret_from_member_blob(
                            &encrypted_secret.secret.ciphertext,
                            &encrypted_secret.secret.nonce,
                            &ephemeral_key,
                            &self_sk,
                        ) {
                            Ok(decrypted_secret) => {
                                info!("Successfully decrypted room secret for member {:?}", member_id);
                                room_data.set_secret(decrypted_secret, current_version);
                            }
                            Err(e) => {
                                warn!("Failed to decrypt room secret: {}", e);
                            }
                        }
                    } else {
                        warn!("No encrypted secret found for member {:?} at version {}", member_id, current_version);
                    }
                }

                // Set the member's nickname in member_info regardless of whether they were already in the room
                // This ensures the member has corresponding MemberInfo even if they were already a member
                let preferred_nickname_sealed = if room_data.room_state.configuration.configuration.privacy_mode == PrivacyMode::Private {
                    // For private rooms, encrypt the nickname with the room secret
                    if let Some((secret, version)) = room_data.get_secret() {
                        use crate::util::ecies::encrypt_with_symmetric_key;
                        let (ciphertext, nonce) = encrypt_with_symmetric_key(secret, preferred_nickname.as_bytes());
                        SealedBytes::Private {
                            ciphertext,
                            nonce,
                            secret_version: version,
                            declared_len_bytes: preferred_nickname.len() as u32,
                        }
                    } else {
                        warn!("Private room but no secret available for encrypting nickname, using public");
                        SealedBytes::public(preferred_nickname.clone().into_bytes())
                    }
                } else {
                    SealedBytes::public(preferred_nickname.clone().into_bytes())
                };

                let member_info = MemberInfo {
                    member_id,
                    version: 0,
                    preferred_nickname: preferred_nickname_sealed,
                };

                let authorized_member_info =
                    AuthorizedMemberInfo::new_with_member_key(member_info.clone(), &self_sk);

                // Create a Delta from the invitation and merge it to ensure that the
                // relevant information is part of the state

                let invitation_delta = ChatRoomStateV1Delta {
                    configuration: None,
                    bans: None,
                    members: Some(MembersDelta::new(vec![authorized_member.clone()])),
                    member_info: Some(vec![authorized_member_info]),
                    secrets: None,
                    recent_messages: None,
                    upgrade: None,
                };

                // Clone current state to avoid borrow issues during merge
                let current_state = room_data.room_state.clone();

                room_data
                    .room_state
                    .apply_delta(
                        &current_state,
                        &ChatRoomParametersV1 { owner: owner_vk },
                        &Some(invitation_delta),
                    )
                    .expect("Failed to apply invitation delta");
            });

            // Make sure SYNC_INFO is properly set up for this room
            SYNC_INFO.with_mut(|sync_info| {
                // Register the room if it wasn't already registered
                sync_info.register_new_room(owner_vk);

                // DO NOT update the last_synced_state here
                // This will ensure the room is marked as needing an update in the next synchronization

                // Update the sync status
                sync_info.update_sync_status(&owner_vk, RoomSyncStatus::Subscribed);
            });

            // Now subscribe to the contract
            let subscribe_result = room_synchronizer.subscribe_to_contract(&key).await;

            if let Err(e) = subscribe_result {
                error!("Failed to subscribe to contract after GET: {}", e);
                // Update the sync status to error
                SYNC_INFO
                    .write()
                    .update_sync_status(&owner_vk, RoomSyncStatus::Error(e.to_string()));
            } else {
                // Mark the invitation as subscribed and retrieved
                PENDING_INVITES.with_mut(|pending_invites| {
                    if let Some(join) = pending_invites.map.get_mut(&owner_vk) {
                        join.status = PendingRoomStatus::Subscribed;
                    }
                });

                // Mark initial sync complete for notifications
                mark_initial_sync_complete(&owner_vk);
            }

            // Dispatch an event to notify the UI
            if let Some(window) = web_sys::window() {
                let key_hex = owner_vk
                    .as_bytes()
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>();
                let event = web_sys::CustomEvent::new("river-invitation-accepted").unwrap();

                // Set the detail property
                js_sys::Reflect::set(
                    &event,
                    &wasm_bindgen::JsValue::from_str("detail"),
                    &wasm_bindgen::JsValue::from_str(&key_hex),
                )
                .unwrap();

                window.dispatch_event(&event).unwrap();

                // Set the current room to the newly accepted room
                CURRENT_ROOM.with_mut(|current_room| {
                    current_room.owner_key = Some(owner_vk);
                });

                // Migrate the signing key to delegate for this new room
                let signing_key_clone = self_sk.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let room_key = owner_vk.to_bytes();
                    let migrated =
                        crate::signing::migrate_signing_key(room_key, &signing_key_clone).await;
                    if migrated {
                        ROOMS.with_mut(|rooms| {
                            if let Some(room_data) = rooms.map.get_mut(&owner_vk) {
                                room_data.key_migrated_to_delegate = true;
                                info!("Signing key migrated to delegate for new room");
                            }
                        });
                    }
                });

                // Mark room as needing sync so it gets saved to delegate storage
                use crate::components::app::NEEDS_SYNC;
                NEEDS_SYNC.write().insert(owner_vk);

                // Trigger synchronization to send the member update to the network
                // This is critical for other users to see that this user has joined
                info!("Triggering synchronization after accepting invitation to propagate member addition");
                use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerMessage;
                use crate::components::app::SYNCHRONIZER;

                if let Err(e) = SYNCHRONIZER
                    .read()
                    .get_message_sender()
                    .unbounded_send(SynchronizerMessage::ProcessRooms)
                {
                    error!(
                        "Failed to trigger synchronization after joining room: {}",
                        e
                    );
                } else {
                    info!("Successfully triggered synchronization after joining room");
                }
            }
        } else if is_existing_room {
            // This is a refresh GET for an already-subscribed room (e.g., after wake from suspension)
            info!("Processing GET response for existing room (refresh after suspension)");
            let retrieved_state: ChatRoomStateV1 = from_cbor_slice::<ChatRoomStateV1>(&state);

            ROOMS.with_mut(|rooms| {
                if let Some(room_data) = rooms.map.get_mut(&owner_vk) {
                    // Create parameters for merge
                    let params = ChatRoomParametersV1 { owner: owner_vk };

                    // Clone current state to avoid borrow issues during merge
                    let current_state = room_data.room_state.clone();

                    // Merge the retrieved state into the existing state
                    match room_data
                        .room_state
                        .merge(&current_state, &params, &retrieved_state)
                    {
                        Ok(_) => {
                            info!(
                                "Successfully merged refreshed state for room {:?}",
                                MemberId::from(owner_vk)
                            );
                        }
                        Err(e) => {
                            error!(
                                "Failed to merge refreshed state for room {:?}: {}",
                                MemberId::from(owner_vk),
                                e
                            );
                        }
                    }
                }
            });

            // Update sync info to reflect we received fresh state
            SYNC_INFO.with_mut(|sync_info| {
                sync_info.update_sync_status(&owner_vk, RoomSyncStatus::Subscribed);
            });
        }
    }

    Ok(())
}
