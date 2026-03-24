use crate::components::app::freenet_api::error::SynchronizerError;
use crate::components::app::freenet_api::room_synchronizer::RoomSynchronizer;
use crate::components::app::notifications::mark_initial_sync_complete;
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{CURRENT_ROOM, PENDING_INVITES, ROOMS, WEB_API};
use crate::constants::ROOM_CONTRACT_WASM;
use crate::invites::PendingRoomStatus;
use crate::room_data::RoomData;
use crate::util::ecies::{decrypt_secret_from_member_blob, decrypt_with_symmetric_key};
use crate::util::{
    from_cbor_slice, get_current_system_time, owner_vk_to_contract_key, to_cbor_vec,
};
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::ReadableExt;
use freenet_scaffold::ComposableState;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
use freenet_stdlib::prelude::{
    ContractCode, ContractContainer, ContractKey, ContractWasmAPIVersion, Parameters,
    WrappedContract, WrappedState,
};
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageId, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::{PrivacyMode, SealedBytes};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::collections::HashMap;
use std::sync::Arc;
use x25519_dalek::PublicKey as X25519PublicKey;

pub async fn handle_get_response(
    _room_synchronizer: &mut RoomSynchronizer,
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

            // Clone self_sk before moving into defer closure, since it's needed later for signing key migration
            let self_sk_for_migration = self_sk.clone();
            // Clone retrieved_state before it's moved into the defer closure,
            // since we need it for the PUT request below
            let retrieved_state_for_put = retrieved_state.clone();

            // Update the room data
            crate::util::defer(move || {
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
                        last_read_message_id: None,
                        secrets: std::collections::HashMap::new(),
                        current_secret_version: None,
                        last_secret_rotation: None,
                        key_migrated_to_delegate: false, // Will be checked/migrated on startup
                        self_authorized_member: None,
                        invite_chain: vec![],
                        self_member_info: None,
                        previous_contract_key: None,
                    }
                });

                // Clear previous_contract_key on successful GET — proves migration worked
                if room_data.previous_contract_key.is_some() {
                    room_data.previous_contract_key = None;
                }

                // If the room already existed, update self_sk and merge state
                if !is_new_entry {
                    // Only update self_sk if the user is NOT the room owner,
                    // to avoid stripping owner privileges
                    if room_data.self_sk.verifying_key() != owner_vk {
                        room_data.self_sk = self_sk.clone();
                        // Reset migration flag so the new key gets migrated
                        room_data.key_migrated_to_delegate = false;
                    }

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

                // Decrypt ALL room secret versions if this is a private room
                if room_data.room_state.configuration.configuration.privacy_mode == PrivacyMode::Private {
                    let current_version = room_data.room_state.secrets.current_version;

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
                                    info!("Successfully decrypted room secret version {} for member {:?}", version, member_id);
                                    room_data.set_secret(decrypted_secret, version);
                                }
                                Err(e) => {
                                    warn!("Failed to decrypt room secret version {}: {}", version, e);
                                }
                            }
                        }
                    }

                    // Ensure current_secret_version is set to the actual current version
                    room_data.current_secret_version = Some(current_version);
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

                // Store membership credentials for future rejoin after
                // inactivity pruning.
                room_data.self_authorized_member = Some(authorized_member.clone());
                room_data.self_member_info = Some(authorized_member_info.clone());
                // Capture invite chain from current state
                if let Ok(chain) = room_data.room_state.members.get_invite_chain(
                    &authorized_member,
                    &ChatRoomParametersV1 { owner: owner_vk },
                ) {
                    room_data.invite_chain = chain;
                }

                // Apply membership immediately on invitation acceptance so
                // that other room members see "X joined the room" right away
                // (not deferred until the user's first message).
                let self_vk = room_data.self_sk.verifying_key();
                let already_member = self_vk == owner_vk
                    || room_data
                        .room_state
                        .members
                        .members
                        .iter()
                        .any(|m| m.member.member_vk == self_vk);

                if !already_member {
                    // Add member + any missing invite chain members
                    let current_member_ids: std::collections::HashSet<_> = room_data
                        .room_state
                        .members
                        .members
                        .iter()
                        .map(|m| m.member.id())
                        .collect();

                    room_data
                        .room_state
                        .members
                        .members
                        .push(authorized_member.clone());
                    for chain_member in &room_data.invite_chain {
                        if !current_member_ids.contains(&chain_member.member.id()) {
                            room_data
                                .room_state
                                .members
                                .members
                                .push(chain_member.clone());
                        }
                    }

                    // Add member info
                    room_data
                        .room_state
                        .member_info
                        .member_info
                        .push(authorized_member_info);

                    // Create and sign join event message — this also keeps
                    // the member from being pruned by post_apply_cleanup
                    // (which retains members who have messages).
                    let join_msg = MessageV1 {
                        room_owner: MemberId::from(owner_vk),
                        author: MemberId::from(&self_vk),
                        content: RoomMessageBody::join_event(),
                        time: get_current_system_time(),
                    };
                    let auth_join =
                        AuthorizedMessageV1::new(join_msg, &room_data.self_sk);
                    room_data
                        .room_state
                        .recent_messages
                        .messages
                        .push(auth_join);
                }

                // Rebuild actions_state from action messages (edit, delete, reaction)
                // This is needed because actions_state is #[serde(skip)] and not serialized
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
            });
            });

            // Make sure SYNC_INFO is properly set up for this room
            crate::util::defer(move || {
                SYNC_INFO.with_mut(|sync_info| {
                    sync_info.register_new_room(owner_vk);
                    // Set to Subscribing — will become Subscribed when handle_put_response()
                    // in put_response.rs processes the PUT reply
                    sync_info.update_sync_status(&owner_vk, RoomSyncStatus::Subscribing);
                });
            });

            // PUT the contract with bundled WASM + subscribe in one request.
            // This registers the contract code and parameters with the local node,
            // which is required for subsequent UPDATEs (sending messages) to succeed.
            // Without this PUT, the node has the state but not the contract code,
            // causing all UPDATEs to fail with "missing contract parameters".
            let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
            let parameters = ChatRoomParametersV1 { owner: owner_vk };
            let params_bytes = to_cbor_vec(&parameters);
            let parameters = Parameters::from(params_bytes);

            let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
                WrappedContract::new(Arc::new(contract_code), parameters),
            ));

            let wrapped_state = WrappedState::new(to_cbor_vec(&retrieved_state_for_put));

            let put_request = ContractRequest::Put {
                contract: contract_container,
                state: wrapped_state,
                related_contracts: Default::default(),
                subscribe: true,
                blocking_subscribe: false,
            };

            let put_result = if let Some(web_api) = WEB_API.write().as_mut() {
                match web_api.send(ClientRequest::ContractOp(put_request)).await {
                    Ok(_) => {
                        info!(
                            "Sent PUT+subscribe for invited room {:?}",
                            MemberId::from(owner_vk)
                        );
                        Ok(())
                    }
                    Err(e) => {
                        error!(
                            "Failed to PUT contract for invited room {:?}: {}",
                            MemberId::from(owner_vk),
                            e
                        );
                        Err(e.to_string())
                    }
                }
            } else {
                Err("WebAPI not available".to_string())
            };

            if let Err(e) = put_result {
                crate::util::defer(move || {
                    SYNC_INFO
                        .write()
                        .update_sync_status(&owner_vk, RoomSyncStatus::Error(e.clone()));
                });
                // Reset invite status so the normal retry flow can pick it up
                crate::util::defer(move || {
                    PENDING_INVITES.with_mut(|pending_invites| {
                        if let Some(join) = pending_invites.map.get_mut(&owner_vk) {
                            join.status = PendingRoomStatus::PendingSubscription;
                        }
                    });
                });
            } else {
                // PUT was sent successfully — proceed with UI updates and key migration.
                // Subscription confirmation happens when handle_put_response() in
                // put_response.rs processes the reply from the node.

                // Mark initial sync complete for notifications
                crate::util::defer(move || {
                    mark_initial_sync_complete(&owner_vk);
                });

                // Close the invitation modal by updating PENDING_INVITES directly.
                // PENDING_INVITES is a GlobalSignal — writing to it re-renders the modal.
                crate::util::defer(move || {
                    PENDING_INVITES.with_mut(|pending| {
                        if let Some(join) = pending.map.get_mut(&owner_vk) {
                            join.status = PendingRoomStatus::Subscribed;
                            info!(
                                "Marked invitation as Subscribed for {:?}",
                                MemberId::from(owner_vk)
                            );
                        }
                    });
                    CURRENT_ROOM.with_mut(|current_room| {
                        current_room.owner_key = Some(owner_vk);
                    });
                });

                {
                    // Migrate the signing key to delegate for this new room
                    let signing_key_clone = self_sk_for_migration.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        let room_key = owner_vk.to_bytes();
                        let result =
                            crate::signing::migrate_signing_key(room_key, &signing_key_clone).await;
                        if result != crate::signing::MigrationResult::Failed {
                            // Must defer signal mutations from spawn_local to
                            // avoid RefCell already borrowed panics in Dioxus runtime
                            crate::util::defer(move || {
                                let mut sanitized = false;
                                ROOMS.with_mut(|rooms| {
                                    if let Some(room_data) = rooms.map.get_mut(&owner_vk) {
                                        room_data.key_migrated_to_delegate = true;
                                        let params = river_core::room_state::ChatRoomParametersV1 {
                                            owner: owner_vk,
                                        };
                                        let removed = crate::signing::remove_unverifiable_messages(
                                            &mut room_data.room_state,
                                            &params,
                                        );
                                        sanitized = removed > 0;
                                        info!("Signing key migrated to delegate for new room");
                                    }
                                });
                                if sanitized {
                                    crate::components::app::mark_needs_sync(owner_vk);
                                }
                            });
                        }
                    });

                    // Mark room as needing sync so it gets saved to delegate storage
                    // and the membership + join event get published to the contract.
                    crate::components::app::mark_needs_sync(owner_vk);
                }
            }
        } else if is_existing_room {
            // Imported rooms use GET-first because their default state has an
            // invalid configuration signature. After merging the retrieved state,
            // we need to PUT+subscribe with the valid state.
            let needs_put_subscribe = ROOMS
                .read()
                .map
                .get(&owner_vk)
                .is_some_and(|rd| rd.is_awaiting_initial_sync())
                && SYNC_INFO
                    .read()
                    .get_sync_status(&owner_vk)
                    .is_some_and(|s| *s == RoomSyncStatus::Subscribing);

            info!(
                "Processing GET response for existing room (needs_put_subscribe={})",
                needs_put_subscribe
            );
            let retrieved_state: ChatRoomStateV1 = from_cbor_slice::<ChatRoomStateV1>(&state);
            let retrieved_state_for_put = retrieved_state.clone();

            crate::util::defer(move || {
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
                                // Note: we intentionally do NOT record receive times here.
                                // GET responses don't reflect real-time message arrival —
                                // we don't know when these messages actually propagated
                                // to our node. Only subscription UPDATE notifications
                                // capture the true arrival moment.

                                // Migration: capture self membership data for old rooms
                                room_data.capture_self_membership_data(&params);
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
            });

            if needs_put_subscribe {
                // This is an imported room that just received its first real state via
                // GET. Now PUT the valid state with subscribe=true to register the
                // contract code and establish a subscription.
                //
                // Note: we PUT `retrieved_state_for_put` (the raw GET response), not the
                // merged ROOMS state, because the deferred merge (above) hasn't run yet
                // (setTimeout(0)). This is correct — the local default state has no useful
                // data to contribute, so the network state IS the valid state.
                info!(
                    "Imported room {:?} received state via GET, now PUTting with subscribe",
                    MemberId::from(owner_vk)
                );

                let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
                let parameters = ChatRoomParametersV1 { owner: owner_vk };
                let params_bytes = to_cbor_vec(&parameters);
                let parameters = Parameters::from(params_bytes);

                let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
                    WrappedContract::new(Arc::new(contract_code), parameters),
                ));

                let wrapped_state = WrappedState::new(to_cbor_vec(&retrieved_state_for_put));

                let put_request = ContractRequest::Put {
                    contract: contract_container,
                    state: wrapped_state,
                    related_contracts: Default::default(),
                    subscribe: true,
                    blocking_subscribe: false,
                };

                let put_succeeded = if let Some(web_api) = WEB_API.write().as_mut() {
                    match web_api.send(ClientRequest::ContractOp(put_request)).await {
                        Ok(_) => {
                            info!(
                                "Sent PUT+subscribe for imported room {:?}",
                                MemberId::from(owner_vk)
                            );
                            true
                        }
                        Err(e) => {
                            error!(
                                "Failed to PUT contract for imported room {:?}: {}",
                                MemberId::from(owner_vk),
                                e
                            );
                            // Reset to Disconnected so the retry loop can pick it up.
                            // After GET+merge the state is valid, so the next attempt
                            // will take the normal PUT path (is_awaiting_initial_sync
                            // returns false once members are populated).
                            crate::util::defer(move || {
                                SYNC_INFO.with_mut(|sync_info| {
                                    sync_info.update_sync_status(
                                        &owner_vk,
                                        RoomSyncStatus::Disconnected,
                                    );
                                });
                            });
                            false
                        }
                    }
                } else {
                    false
                };

                if put_succeeded {
                    // Trigger signing key migration now that we have valid state
                    let self_sk_opt: Option<ed25519_dalek::SigningKey> = {
                        let rooms = ROOMS.read();
                        rooms.map.get(&owner_vk).map(|rd| rd.self_sk.clone())
                    };
                    if let Some(self_sk) = self_sk_opt {
                        wasm_bindgen_futures::spawn_local(async move {
                            let room_key = owner_vk.to_bytes();
                            let result =
                                crate::signing::migrate_signing_key(room_key, &self_sk).await;
                            if result != crate::signing::MigrationResult::Failed {
                                crate::util::defer(move || {
                                    ROOMS.with_mut(|rooms| {
                                        if let Some(room_data) = rooms.map.get_mut(&owner_vk) {
                                            room_data.key_migrated_to_delegate = true;
                                            let params = ChatRoomParametersV1 { owner: owner_vk };
                                            let removed =
                                                crate::signing::remove_unverifiable_messages(
                                                    &mut room_data.room_state,
                                                    &params,
                                                );
                                            if removed > 0 {
                                                crate::components::app::mark_needs_sync(owner_vk);
                                            }
                                        }
                                    });
                                });
                            }
                        });
                    }

                    // Persist merged state to delegate and mark sync complete
                    crate::components::app::mark_needs_sync(owner_vk);
                    crate::util::defer(move || {
                        mark_initial_sync_complete(&owner_vk);
                    });
                }
            } else {
                // Normal refresh — already subscribed, just update sync info
                crate::util::defer(move || {
                    SYNC_INFO.with_mut(|sync_info| {
                        sync_info.update_sync_status(&owner_vk, RoomSyncStatus::Subscribed);
                    });
                });
            }
        }
    }

    Ok(())
}
