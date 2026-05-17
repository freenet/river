use crate::components::app::freenet_api::error::SynchronizerError;
use crate::components::app::freenet_api::room_synchronizer::RoomSynchronizer;
use crate::components::app::notifications::mark_initial_sync_complete;
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{CURRENT_ROOM, PENDING_INVITES, ROOMS, WEB_API};
use crate::constants::ROOM_CONTRACT_WASM;
use crate::invites::PendingRoomStatus;
use crate::room_data::RoomData;
use crate::util::ecies::decrypt_with_symmetric_key;
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
            // Build the state that goes on the wire in the PUT, BEFORE the
            // defer block (the defer runs asynchronously, so any mutation
            // it makes to ROOMS[owner_vk].room_state can't be observed by
            // the PUT-construction code that follows it).
            //
            // The PUT must include the invitee's join_event so the
            // owner-side contract sees them as an active member
            // immediately — without this, the owner's post_apply_cleanup
            // would prune the freshly-PUT invitee until the next sync
            // delta lands carrying their join_event. See Bug #3 PR B
            // (Ivvor 2026-05-17) and issue #110.
            //
            // We also return the synthesised join_event so the deferred
            // ROOMS mutation can use the BYTE-IDENTICAL message (same
            // timestamp, same signature, same MessageId) — otherwise
            // the local state and the PUT state would each carry a
            // separately-signed join_event with different IDs, leaving
            // the room with two "joined" entries after the sync settles
            // (Codex review of this PR).
            let (retrieved_state_for_put, synthesised_join_event) = build_state_for_put(
                retrieved_state.clone(),
                owner_vk,
                &self_sk,
                &authorized_member,
            );

            // Update the room data
            crate::util::defer(move || {
                ROOMS.with_mut(|rooms| {
                // Accepting an invitation is an explicit rejoin — clear any
                // prior leave tombstone for this room so the merge path
                // doesn't silently filter the room out again later. See
                // freenet/river#247.
                rooms.removed_rooms.remove(&owner_vk);

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
                let decrypted = room_data.repopulate_secrets_from_state();
                if room_data.is_private() {
                    info!(
                        "GET response: decrypted {} room secret(s) for member {:?}",
                        decrypted, member_id
                    );
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

                    // Append the same join_event we already injected into
                    // the PUT payload (see `build_state_for_put`). Reusing
                    // the exact `AuthorizedMessageV1` — same timestamp,
                    // same signature, same MessageId — is critical: if we
                    // signed a NEW join_event here with a fresh timestamp,
                    // the local state and the PUT state would each carry a
                    // separately-IDed "joined" entry, and the room would
                    // surface two join events for a single acceptance once
                    // the network state syncs back. See Codex review of
                    // PR #272.
                    if let Some(auth_join) = synthesised_join_event.clone() {
                        room_data
                            .room_state
                            .recent_messages
                            .messages
                            .push(auth_join);
                    }
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
                        let params = ChatRoomParametersV1 { owner: owner_vk };

                        // Note: we intentionally do NOT record receive times here.
                        // GET responses don't reflect real-time message arrival —
                        // we don't know when these messages actually propagated
                        // to our node. Only subscription UPDATE notifications
                        // capture the true arrival moment.

                        if room_data.is_awaiting_initial_sync() {
                            // Imported rooms have a placeholder default state with
                            // owner_member_id: FastHash(0). Merging fails because
                            // the retrieved state has the real owner's member ID
                            // and apply_delta rejects owner_member_id changes.
                            // Replace the state wholesale — the default has no
                            // useful data to preserve.
                            info!(
                                "Replacing placeholder state for imported room {:?} with network state",
                                MemberId::from(owner_vk)
                            );
                            room_data.room_state = retrieved_state;
                            room_data.capture_self_membership_data(&params);
                            // #251: a refresh/suspension GET on an imported room
                            // may be the first state arrival carrying our
                            // encrypted_secrets back-fill. The wholesale
                            // `room_state = retrieved_state` above does NOT
                            // touch the in-memory `secrets` HashMap (that's a
                            // separate #[serde(skip)] field on `RoomData`), so
                            // any stale entries from a previous state would
                            // linger; repopulate decrypts whatever versions
                            // the new state carries for us, and the contains-
                            // key guard inside the helper makes lingering
                            // entries from a prior state harmless.
                            let _ = room_data.repopulate_secrets_from_state();
                        } else {
                            let current_state = room_data.room_state.clone();
                            match room_data
                                .room_state
                                .merge(&current_state, &params, &retrieved_state)
                            {
                                Ok(_) => {
                                    info!(
                                        "Successfully merged refreshed state for room {:?}",
                                        MemberId::from(owner_vk)
                                    );
                                    room_data.capture_self_membership_data(&params);
                                    // #251: the refresh/suspension GET may be
                                    // the first response carrying a
                                    // newly-back-filled or newly-rotated
                                    // encrypted_secrets blob for us. Without
                                    // this, the in-memory `secrets` map stays
                                    // stale until the next subscription update.
                                    let _ = room_data.repopulate_secrets_from_state();
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
                            // returns false once config has a valid owner signature).
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

/// Build the state payload to PUT after accepting an invitation.
///
/// The PUT must include the invitee's join_event message so the
/// owner-side contract sees them as an active author immediately. The
/// in-memory mutation that adds this message lives inside an
/// asynchronous `defer()` block, so the PUT path (which runs
/// synchronously, before the deferred closure ever executes) cannot
/// observe it via `ROOMS`. We mirror the same mutation here, producing
/// a state value that goes straight on the wire.
///
/// Without this, the invitee is silently pruned by the owner's
/// `post_apply_cleanup` until the next sync delta lands carrying the
/// invitee's join_event — the underlying cause of the
/// "newly-invited member silently dropped" symptom (Bug #3, issue
/// #110, Ivvor 2026-05-17).
///
/// Returns the new state plus, when a join_event was synthesised, the
/// `AuthorizedMessageV1` itself. The deferred ROOMS mutation MUST
/// reuse this same message (rather than signing a fresh one with a
/// new timestamp) — otherwise the local state and the PUT state
/// would each carry a different-IDed join_event for the same
/// acceptance, leaving the room with duplicate "joined" entries after
/// the sync settles. See Codex review of PR #272.
///
/// Pure function — no I/O, no signal access — so it's directly
/// unit-testable without a Dioxus runtime.
pub(crate) fn build_state_for_put(
    mut state: ChatRoomStateV1,
    owner_vk: ed25519_dalek::VerifyingKey,
    invitee_sk: &ed25519_dalek::SigningKey,
    authorized_member: &river_core::room_state::member::AuthorizedMember,
) -> (ChatRoomStateV1, Option<AuthorizedMessageV1>) {
    let self_vk = invitee_sk.verifying_key();

    // The owner accepts their own state as-is — no synthesised
    // join_event, no member injection.
    if self_vk == owner_vk {
        return (state, None);
    }

    let already_member = state
        .members
        .members
        .iter()
        .any(|m| m.member.member_vk == self_vk);
    if already_member {
        return (state, None);
    }

    // Refuse to inject the invitee if doing so would push the full-state
    // `members.len()` over `max_members`. Direct PUT bypasses the delta
    // path's `MembersV1::remove_excess_members` trim, so the contract's
    // `validate_state` (which calls `MembersV1::verify`, which rejects
    // `members.len() > max_members`) would refuse the PUT and the
    // invitation would never complete. For at-capacity rooms we fall
    // back to the pre-PR-B behaviour — PUT the retrieved state as-is and
    // let the natural delta path either trim a stale member or report
    // the failure. See Codex review of PR #272 (second pass).
    let max_members = state.configuration.configuration.max_members;
    if state.members.members.len() >= max_members {
        return (state, None);
    }

    state.members.members.push(authorized_member.clone());

    let join_msg = MessageV1 {
        room_owner: MemberId::from(owner_vk),
        author: MemberId::from(&self_vk),
        content: RoomMessageBody::join_event(),
        time: get_current_system_time(),
    };
    let auth_join = AuthorizedMessageV1::new(join_msg, invitee_sk);
    state.recent_messages.messages.push(auth_join.clone());

    (state, Some(auth_join))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::member::{AuthorizedMember, Member};

    /// Regression test for Bug #3 PR B (issue #110, Ivvor 2026-05-17):
    /// the state PUT to the network after accepting an invitation must
    /// contain the invitee's synthesised join_event. Without it, the
    /// owner's `post_apply_cleanup` prunes the invitee from members on
    /// the very first state ingestion (no authored messages, no DMs).
    #[test]
    fn build_state_for_put_includes_synthesised_join_event() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let invitee_vk = invitee_sk.verifying_key();

        // Pre-acceptance state: owner config only, invitee not yet in
        // members. This matches what the invitee fetches via GET — the
        // owner hasn't added them yet, they're authenticating via their
        // invitation token.
        let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk);
        let state = ChatRoomStateV1 {
            configuration: config,
            ..Default::default()
        };

        let authorized_member = AuthorizedMember::new(
            Member {
                owner_member_id: owner_vk.into(),
                invited_by: owner_vk.into(),
                member_vk: invitee_vk,
            },
            &owner_sk,
        );

        let (put_state, synthesised_join_event) =
            build_state_for_put(state, owner_vk, &invitee_sk, &authorized_member);

        // Member is in the state.
        assert!(
            put_state
                .members
                .members
                .iter()
                .any(|m| m.member.member_vk == invitee_vk),
            "PUT state must include the invitee as a member"
        );
        // join_event is in recent_messages (matched by content type
        // — RoomMessageBody represents events as Public messages with
        // CONTENT_TYPE_EVENT, not a dedicated variant).
        let join_present = put_state.recent_messages.messages.iter().any(|m| {
            m.message.author == MemberId::from(&invitee_vk) && m.message.content.is_event()
        });
        assert!(
            join_present,
            "PUT state must include the invitee's join_event so the owner-side \
             post_apply_cleanup doesn't prune them on first ingestion"
        );

        // The returned synthesised join_event must match the one in the
        // PUT state byte-for-byte. The defer block uses this exact
        // message to keep the local state and the PUT state in sync —
        // re-signing a fresh one with a new timestamp would leave the
        // room with duplicate "joined" entries.
        let returned = synthesised_join_event
            .as_ref()
            .expect("non-owner path must return the synthesised join_event");
        let in_state_join = put_state
            .recent_messages
            .messages
            .iter()
            .find(|m| {
                m.message.author == MemberId::from(&invitee_vk) && m.message.content.is_event()
            })
            .expect("join_event must be in state");
        assert_eq!(
            returned.id(),
            in_state_join.id(),
            "returned join_event MessageId must match the one in the PUT state"
        );

        // And critically: when we run post_apply_cleanup on this state
        // (as the owner-side contract would on receiving the PUT),
        // the invitee must SURVIVE — not be pruned for inactivity.
        let mut after_cleanup = put_state;
        let params = ChatRoomParametersV1 { owner: owner_vk };
        after_cleanup
            .post_apply_cleanup(&params)
            .expect("post_apply_cleanup must succeed on valid state");
        assert!(
            after_cleanup
                .members
                .members
                .iter()
                .any(|m| m.member.member_vk == invitee_vk),
            "invitee must survive owner-side post_apply_cleanup — that's the whole \
             point of including the join_event in the PUT"
        );
    }

    /// Owner PUTting their own state must NOT have anything synthesised
    /// — they're not joining their own room.
    #[test]
    fn build_state_for_put_owner_path_is_noop() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();

        let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk);
        let state = ChatRoomStateV1 {
            configuration: config,
            ..Default::default()
        };

        // Use a dummy authorized_member with the owner's VK — the owner
        // path returns early before reading it.
        let authorized_member = AuthorizedMember::new(
            Member {
                owner_member_id: owner_vk.into(),
                invited_by: owner_vk.into(),
                member_vk: owner_vk,
            },
            &owner_sk,
        );

        let (put_state, synthesised_join_event) =
            build_state_for_put(state.clone(), owner_vk, &owner_sk, &authorized_member);

        assert!(
            put_state.members.members.is_empty(),
            "owner path must not inject members"
        );
        assert!(
            put_state.recent_messages.messages.is_empty(),
            "owner path must not inject join_event"
        );
        assert!(
            synthesised_join_event.is_none(),
            "owner path must not return a synthesised join_event"
        );
    }

    /// Regression test for the second pass of Codex review on PR #272:
    /// when the room is already at `max_members`, `build_state_for_put`
    /// must NOT push the invitee onto the full-state PUT. Direct PUT
    /// bypasses `MembersV1::remove_excess_members`, and `validate_state`
    /// (`MembersV1::verify`) rejects `members.len() > max_members`, so
    /// the contract would refuse the PUT and the invitation would never
    /// complete.
    #[test]
    fn build_state_for_put_respects_max_members() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let invitee_vk = invitee_sk.verifying_key();

        // Configure a tiny room (cap = 1) and seed it with one existing
        // member so adding the invitee would push to 2 > cap.
        let mut config = Configuration::default();
        config.max_members = 1;
        let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

        let existing_sk = SigningKey::generate(&mut rng);
        let existing = AuthorizedMember::new(
            Member {
                owner_member_id: owner_vk.into(),
                invited_by: owner_vk.into(),
                member_vk: existing_sk.verifying_key(),
            },
            &owner_sk,
        );
        let state = ChatRoomStateV1 {
            configuration: auth_config,
            members: river_core::room_state::member::MembersV1 {
                members: vec![existing],
            },
            ..Default::default()
        };

        let authorized_member = AuthorizedMember::new(
            Member {
                owner_member_id: owner_vk.into(),
                invited_by: owner_vk.into(),
                member_vk: invitee_vk,
            },
            &owner_sk,
        );

        let (put_state, synthesised) =
            build_state_for_put(state, owner_vk, &invitee_sk, &authorized_member);

        assert_eq!(
            put_state.members.members.len(),
            1,
            "must not push past max_members in the full-state PUT"
        );
        assert!(
            !put_state
                .members
                .members
                .iter()
                .any(|m| m.member.member_vk == invitee_vk),
            "invitee must not be in the PUT state when room is at capacity"
        );
        assert!(
            synthesised.is_none(),
            "no synthesised join_event when we skipped the member injection"
        );

        // The PUT state must still be valid (no over-cap violation).
        let params = ChatRoomParametersV1 { owner: owner_vk };
        put_state
            .verify(&put_state, &params)
            .expect("at-capacity fallback state must verify");
    }
}
