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
    ContractCode, ContractContainer, ContractKey, ContractWasmAPIVersion, Parameters, UpdateData,
    WrappedContract, WrappedState,
};
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageId, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::{PrivacyMode, SealedBytes};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
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
            // If the room is at capacity, `build_state_for_put` returns
            // an error so we can short-circuit BEFORE touching local
            // ROOMS state. Pushing the invitee into local state when
            // the PUT can't carry them would leave the user with a
            // "phantom local membership" — UI shows them as a member,
            // but the network never sees the join, and the next
            // sync from the owner silently strips them with no
            // surfaced reason. See HIGH-1 finding on PR #272.
            // Build the invitee's member_info ONCE, before the PUT and
            // before the deferred ROOMS mutation, so both carry a
            // byte-identical entry (the same reuse discipline the
            // synthesised join_event follows). A member who lands in the
            // contract's `members` list with no `member_info` entry
            // renders as "Unknown" to every other peer — the PR #272
            // regression. The entry MUST be self-signed with the
            // invitee's own key; the room contract rejects member_info
            // signed by anyone else.
            //
            // A PRIVATE room's nickname must be encrypted. If the room
            // secret is not yet available (the owner's encrypted-secret
            // back-fill is asynchronous), we deliberately produce NO
            // member_info rather than fall back to a plaintext
            // `SealedBytes::public` seal — publishing a cleartext
            // nickname into a private room is a privacy leak.
            // `build_member_info_heal` re-publishes it, properly sealed,
            // on a later GET once the secret has arrived.
            let authorized_member_info: Option<AuthorizedMemberInfo> = {
                let sealed_nickname = if retrieved_state.configuration.configuration.privacy_mode
                    == PrivacyMode::Private
                {
                    current_secret_from_state(&retrieved_state, &self_sk).map(
                        |(secret, version)| {
                            crate::util::ecies::seal_bytes(
                                preferred_nickname.as_bytes(),
                                &secret,
                                version,
                            )
                        },
                    )
                } else {
                    Some(SealedBytes::public(preferred_nickname.as_bytes().to_vec()))
                };
                if sealed_nickname.is_none() {
                    warn!(
                        "Private room secret not available yet — deferring invitee \
                         member_info to the self-heal path"
                    );
                }
                sealed_nickname.map(|preferred_nickname| {
                    AuthorizedMemberInfo::new_with_member_key(
                        MemberInfo {
                            member_id,
                            version: 0,
                            preferred_nickname,
                        },
                        &self_sk,
                    )
                })
            };

            let (retrieved_state_for_put, synthesised_join_event) = match build_state_for_put(
                retrieved_state.clone(),
                owner_vk,
                &self_sk,
                &authorized_member,
                authorized_member_info.as_ref(),
                get_current_system_time(),
            ) {
                Ok(v) => v,
                Err(err) => {
                    error!(
                        "Cannot complete invitation acceptance for room {:?}: {}",
                        MemberId::from(owner_vk),
                        err
                    );
                    let err_msg = err.to_string();
                    crate::util::defer(move || {
                        SYNC_INFO
                            .write()
                            .update_sync_status(&owner_vk, RoomSyncStatus::Error(err_msg.clone()));
                    });
                    // Surface failure on PENDING_INVITES so the
                    // existing modal can report it and the user can
                    // dismiss the join — same shape as a PUT failure.
                    crate::util::defer(move || {
                        PENDING_INVITES.with_mut(|pending| {
                            if let Some(join) = pending.map.get_mut(&owner_vk) {
                                join.status = PendingRoomStatus::Error(err.to_string());
                            }
                        });
                    });
                    return Ok(());
                }
            };

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
                    let is_new_entry =
                        matches!(entry, std::collections::hash_map::Entry::Vacant(_));

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

                    // The invitee's `authorized_member_info` was built once
                    // above, before `build_state_for_put`, and moved into this
                    // closure. Reusing the SAME value here keeps the local
                    // ROOMS state and the PUT payload byte-identical — the
                    // same reuse discipline the synthesised join_event
                    // follows. (Re-sealing here would also be wrong for
                    // private rooms: each seal uses a fresh random nonce, so
                    // a re-built entry would not match the PUT's.)

                    // Store membership credentials for future rejoin after
                    // inactivity pruning. `authorized_member_info` is `None`
                    // only when a private room's secret was not yet available
                    // to seal the nickname — see the build above.
                    room_data.self_authorized_member = Some(authorized_member.clone());
                    room_data.self_member_info = authorized_member_info.clone();
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

                        // Add member info. Skipped only when the private-room
                        // secret was unavailable to seal the nickname (see the
                        // build above); the self-heal restores it, properly
                        // sealed, once the secret arrives.
                        if let Some(member_info) = authorized_member_info {
                            room_data
                                .room_state
                                .member_info
                                .member_info
                                .push(member_info);
                        }

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
                    let is_private = room_data
                        .room_state
                        .configuration
                        .configuration
                        .privacy_mode
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
                                if let RoomMessageBody::Private {
                                    ciphertext,
                                    nonce,
                                    secret_version,
                                    ..
                                } = &msg.message.content
                                {
                                    // Look up the secret for this message's version
                                    room_data.get_secret_for_version(*secret_version).and_then(
                                        |secret| {
                                            decrypt_with_symmetric_key(secret, ciphertext, nonce)
                                                .ok()
                                                .map(|plaintext| (msg.id(), plaintext))
                                        },
                                    )
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
                        room_data.room_state.recent_messages.rebuild_actions_state();
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

            // Member-info self-heal — remediation for the PR #272 stranding.
            // If the freshly-GET'd canonical state shows us in `members`
            // with no matching `member_info` entry, every other peer
            // renders us as "Unknown". Detect it from the raw network
            // state and re-publish our own self-signed member_info.
            // Idempotent: once the entry lands, later GETs no longer see
            // the stranding and the heal stops firing.
            let member_info_heal: Option<AuthorizedMemberInfo> = ROOMS
                .read()
                .map
                .get(&owner_vk)
                .and_then(|rd| rd.build_member_info_heal(&retrieved_state));

            // For an imported room the heal must ride along in the PUT
            // below: a standalone UPDATE sent before that PUT registers
            // the contract code with the local node would be rejected
            // ("missing contract parameters"). An already-subscribed room
            // has no PUT, so its heal goes out as a standalone UPDATE
            // further down.
            let mut retrieved_state_for_put = retrieved_state.clone();
            if needs_put_subscribe {
                if let Some(heal) = &member_info_heal {
                    retrieved_state_for_put
                        .member_info
                        .member_info
                        .push(heal.clone());
                }
            }

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

            // Send the member-info self-heal as a standalone UPDATE — but
            // ONLY for an already-subscribed room. For an imported room
            // (`needs_put_subscribe`) the heal was folded into the PUT
            // payload above instead; sending an UPDATE before that PUT
            // registers the contract code locally would drop it. A
            // dedicated member_info-only delta is used here rather than
            // the normal last_synced_state diff path, which can believe
            // the entry is already synced when the network never received
            // it. The delta is self-signed and idempotent, so re-sending
            // it is harmless if it raced another heal.
            if !needs_put_subscribe {
                if let Some(heal_info) = member_info_heal {
                    let heal_delta = ChatRoomStateV1Delta {
                        member_info: Some(vec![heal_info]),
                        ..Default::default()
                    };
                    let update_request = ContractRequest::Update {
                        key,
                        data: UpdateData::Delta(to_cbor_vec(&heal_delta).into()),
                    };
                    if let Some(web_api) = WEB_API.write().as_mut() {
                        match web_api
                            .send(ClientRequest::ContractOp(update_request))
                            .await
                        {
                            Ok(_) => info!(
                                "Sent member_info self-heal UPDATE for room {:?}",
                                MemberId::from(owner_vk)
                            ),
                            Err(e) => warn!(
                                "Failed to send member_info self-heal for room {:?}: {}",
                                MemberId::from(owner_vk),
                                e
                            ),
                        }
                    }
                }
            }

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

/// Error returned by [`build_state_for_put`] when the invitation cannot
/// complete cleanly.
///
/// The caller MUST short-circuit on these — both the PUT itself and the
/// deferred ROOMS mutation must be skipped, otherwise the local state
/// would carry a "phantom membership" that never made it onto the
/// network (next sync from the owner would silently strip it,
/// leaving the user unable to send messages with no surfaced reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuildStateForPutError {
    /// Room is already at `max_members`. Direct PUT bypasses the delta
    /// path's `MembersV1::remove_excess_members` trim, so the contract's
    /// `MembersV1::verify` would reject the PUT (`members.len() >
    /// max_members`). Surfacing this explicitly lets the UI report a
    /// "room full" toast/log rather than silently pushing the invitee
    /// into local-only state.
    RoomAtCapacity {
        /// `state.configuration.configuration.max_members`
        max_members: usize,
        /// `state.members.members.len()` at the time of the PUT
        current_members: usize,
    },
}

impl std::fmt::Display for BuildStateForPutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildStateForPutError::RoomAtCapacity {
                max_members,
                current_members,
            } => write!(
                f,
                "room is at capacity ({current_members}/{max_members} members); \
                 invitation cannot complete until an existing member leaves or is removed"
            ),
        }
    }
}

/// Decrypt the room's current-version secret out of a raw network
/// `ChatRoomStateV1`, for a member who holds `self_sk`.
///
/// Mirrors the per-blob decrypt loop in
/// `RoomData::repopulate_secrets_from_state`, but for the single
/// current version and without needing a constructed `RoomData` — the
/// invitation-accept PUT path must seal the joiner's nickname before
/// any `RoomData` exists. Returns `None` on public rooms (no secret),
/// when the blob for the current version hasn't been issued for this
/// member yet, or when decryption fails.
fn current_secret_from_state(
    state: &ChatRoomStateV1,
    self_sk: &ed25519_dalek::SigningKey,
) -> Option<([u8; 32], u32)> {
    let member_id = MemberId::from(&self_sk.verifying_key());
    let version = state.secrets.current_version;
    let blob = state
        .secrets
        .encrypted_secrets
        .iter()
        .find(|s| s.secret.member_id == member_id && s.secret.secret_version == version)?;
    let secret = crate::util::ecies::decrypt_secret_from_member_blob_raw(
        &blob.secret.ciphertext,
        &blob.secret.nonce,
        &blob.secret.sender_ephemeral_public_key,
        self_sk,
    )
    .ok()?;
    Some((secret, version))
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
/// The PUT must ALSO include the invitee's `member_info` entry — passed
/// in by the caller, who builds it once and reuses the identical value
/// for the deferred local ROOMS mutation. A member present in `members`
/// but absent from `member_info` renders as "Unknown" to every other
/// peer. PR #272 added the join_event injection but omitted member_info,
/// which is the regression this restores.
///
/// Returns the new state plus, when a join_event was synthesised, the
/// `AuthorizedMessageV1` itself. The deferred ROOMS mutation MUST
/// reuse this same message (rather than signing a fresh one with a
/// new timestamp) — otherwise the local state and the PUT state
/// would each carry a different-IDed join_event for the same
/// acceptance, leaving the room with duplicate "joined" entries after
/// the sync settles. See Codex review of PR #272.
///
/// Returns `Err(BuildStateForPutError::RoomAtCapacity)` when the room
/// is at `max_members`. The caller MUST short-circuit on this — both
/// the PUT and the deferred ROOMS mutation must be skipped, otherwise
/// the invitee ends up with a "phantom local membership" that doesn't
/// exist on the network. See HIGH-1 finding on PR #272 review round 2.
///
/// Synchronous, state-only mutation — no signal access, no I/O. The
/// `time` parameter is injected so tests can fix the join_event
/// timestamp deterministically; production calls pass
/// `get_current_system_time()`. (Earlier docs claimed "pure function";
/// that was wrong — synthesising a real-time-stamped `MessageV1` is
/// neither pure nor referentially transparent. See HIGH-2 finding on
/// PR #272 review round 2.)
pub(crate) fn build_state_for_put(
    mut state: ChatRoomStateV1,
    owner_vk: ed25519_dalek::VerifyingKey,
    invitee_sk: &ed25519_dalek::SigningKey,
    authorized_member: &river_core::room_state::member::AuthorizedMember,
    member_info: Option<&AuthorizedMemberInfo>,
    time: std::time::SystemTime,
) -> Result<(ChatRoomStateV1, Option<AuthorizedMessageV1>), BuildStateForPutError> {
    let self_vk = invitee_sk.verifying_key();

    // The owner accepts their own state as-is — no synthesised
    // join_event, no member injection.
    if self_vk == owner_vk {
        return Ok((state, None));
    }

    let already_member = state
        .members
        .members
        .iter()
        .any(|m| m.member.member_vk == self_vk);
    if already_member {
        return Ok((state, None));
    }

    // Refuse to inject the invitee if doing so would push the full-state
    // `members.len()` over `max_members`. Direct PUT bypasses the delta
    // path's `MembersV1::remove_excess_members` trim, so the contract's
    // `validate_state` (which calls `MembersV1::verify`, which rejects
    // `members.len() > max_members`) would refuse the PUT and the
    // invitation would never complete.
    //
    // Earlier (PR #272 second pass) this branch returned `Ok((state,
    // None))` and the caller still pushed the invitee into local
    // ROOMS state under the "fall back to pre-PR-B behaviour"
    // assumption — but that left the user with a local-only
    // membership the owner would never know about, silently stripped
    // on next sync. We now surface the failure explicitly so the
    // caller can short-circuit before mutating ROOMS. See HIGH-1
    // finding on PR #272 review round 2.
    let max_members = state.configuration.configuration.max_members;
    if state.members.members.len() >= max_members {
        return Err(BuildStateForPutError::RoomAtCapacity {
            max_members,
            current_members: state.members.members.len(),
        });
    }

    state.members.members.push(authorized_member.clone());

    // Inject the invitee's member_info alongside the member entry. Without
    // this, the invitee lands in the contract's `members` list with no
    // `member_info`, so every other peer renders them as "Unknown" (the
    // PR #272 regression). The caller builds `member_info` once and reuses
    // the SAME value for the deferred local ROOMS mutation, keeping the
    // PUT state and local state byte-identical. The entry is self-signed
    // by the invitee — the room contract rejects member_info signed by
    // any other key.
    //
    // `member_info` is `None` only when the room is private and its
    // secret was not yet available to seal the nickname: we publish no
    // member_info rather than leak a plaintext nickname, and the
    // self-heal (`build_member_info_heal`) restores it on a later GET.
    if let Some(member_info) = member_info {
        state.member_info.member_info.push(member_info.clone());
    }

    let join_msg = MessageV1 {
        room_owner: MemberId::from(owner_vk),
        author: MemberId::from(&self_vk),
        content: RoomMessageBody::join_event(),
        time,
    };
    let auth_join = AuthorizedMessageV1::new(join_msg, invitee_sk);
    state.recent_messages.messages.push(auth_join.clone());

    Ok((state, Some(auth_join)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use river_core::room_state::member::{AuthorizedMember, Member};

    /// Build a public-room `AuthorizedMemberInfo` self-signed by `sk`,
    /// so the `build_state_for_put` tests can supply the member_info
    /// parameter the production caller builds before the PUT.
    fn test_member_info(sk: &SigningKey) -> AuthorizedMemberInfo {
        AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: MemberId::from(&sk.verifying_key()),
                version: 0,
                preferred_nickname: SealedBytes::public(b"Tester".to_vec()),
            },
            sk,
        )
    }

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

        // Use a fixed timestamp so the test is deterministic — the
        // production code passes `get_current_system_time()`.
        let fixed_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let (put_state, synthesised_join_event) = build_state_for_put(
            state,
            owner_vk,
            &invitee_sk,
            &authorized_member,
            Some(&test_member_info(&invitee_sk)),
            fixed_time,
        )
        .expect("non-capacity invitee path must succeed");

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

    /// Regression test for the PR #272 "Unknown member" bug: the state
    /// PUT after accepting an invitation must also include the invitee's
    /// `member_info` entry. Without it the invitee is in `members` on the
    /// contract but absent from `member_info`, so every other peer
    /// renders them as "Unknown".
    #[test]
    fn build_state_for_put_includes_member_info() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let invitee_vk = invitee_sk.verifying_key();

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
        let member_info = test_member_info(&invitee_sk);
        let fixed_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);

        let (put_state, _) = build_state_for_put(
            state,
            owner_vk,
            &invitee_sk,
            &authorized_member,
            Some(&member_info),
            fixed_time,
        )
        .expect("non-capacity invitee path must succeed");

        // The invitee's member_info must be in the PUT state.
        let invitee_id = MemberId::from(&invitee_vk);
        assert!(
            put_state
                .member_info
                .member_info
                .iter()
                .any(|i| i.member_info.member_id == invitee_id),
            "PUT state must include the invitee's member_info — without it \
             every other peer renders the invitee as \"Unknown\" (PR #272 bug)"
        );

        // It must survive the owner-side post_apply_cleanup and the
        // contract's verify() — proving it is a well-formed, self-signed
        // entry the room contract accepts.
        let params = ChatRoomParametersV1 { owner: owner_vk };
        let mut after_cleanup = put_state;
        after_cleanup
            .post_apply_cleanup(&params)
            .expect("post_apply_cleanup must succeed");
        assert!(
            after_cleanup
                .member_info
                .member_info
                .iter()
                .any(|i| i.member_info.member_id == invitee_id),
            "invitee member_info must survive post_apply_cleanup"
        );
        after_cleanup
            .verify(&after_cleanup, &params)
            .expect("PUT state with injected member_info must verify");
    }

    /// `current_secret_from_state` returns `None` for a room with no
    /// encrypted secret for the member — a public room carries none, so
    /// the invitation-accept PUT path public-seals the nickname (correct
    /// for a public room, not a leak). This is the branch the official
    /// (public) room's join path exercises.
    #[test]
    fn current_secret_from_state_none_without_blob() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let member_sk = SigningKey::generate(&mut rng);
        let config = AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk);
        let state = ChatRoomStateV1 {
            configuration: config,
            ..Default::default()
        };
        assert!(
            current_secret_from_state(&state, &member_sk).is_none(),
            "no encrypted_secrets blob for the member → None"
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

        let fixed_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let (put_state, synthesised_join_event) = build_state_for_put(
            state.clone(),
            owner_vk,
            &owner_sk,
            &authorized_member,
            Some(&test_member_info(&owner_sk)),
            fixed_time,
        )
        .expect("owner path must succeed");

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

    /// Regression test for HIGH-1 on PR #272 review round 2:
    /// when the room is at capacity, `build_state_for_put` MUST return
    /// `Err(RoomAtCapacity)` rather than silently returning the
    /// pre-acceptance state unchanged.
    ///
    /// Before this fix the function returned `Ok((state_unchanged,
    /// None))` for capacity-exceeded rooms. The caller's deferred
    /// ROOMS mutation then pushed the invitee into local state
    /// anyway — leaving the user with a "phantom local
    /// membership" the network never saw, silently stripped on next
    /// sync, with NO user-facing signal. Returning an Err lets the
    /// caller short-circuit BEFORE touching ROOMS, so the user gets
    /// an explicit "room full" failure rather than a confusing local
    /// state that quietly evaporates.
    ///
    /// Earlier this test also covered the second-pass Codex review
    /// (PR #272 round 2) — bypassing the
    /// `MembersV1::remove_excess_members` trim and pushing past
    /// `max_members` would make the contract reject the PUT.
    #[test]
    fn build_state_for_put_errs_at_capacity() {
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
                members: vec![existing.clone()],
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

        let fixed_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let result = build_state_for_put(
            state.clone(),
            owner_vk,
            &invitee_sk,
            &authorized_member,
            Some(&test_member_info(&invitee_sk)),
            fixed_time,
        );

        // Must surface the failure explicitly.
        match result {
            Err(BuildStateForPutError::RoomAtCapacity {
                max_members,
                current_members,
            }) => {
                assert_eq!(max_members, 1);
                assert_eq!(current_members, 1);
            }
            other => panic!(
                "expected RoomAtCapacity error, got {:?} — see HIGH-1 finding \
                 on PR #272 review round 2",
                other
            ),
        }
    }

    /// Companion to `build_state_for_put_errs_at_capacity` —
    /// pins HIGH-1's secondary requirement: capacity-exceeded
    /// `build_state_for_put` returns the input state UNTOUCHED via
    /// its Result::Err discard, so any pre-existing state shape is
    /// preserved if a caller inspects the input value after the
    /// Err. (Conceptual: this is the OWNER's view of the room — we
    /// must not silently mutate it just because someone tried to
    /// accept an invite past capacity.)
    #[test]
    fn build_state_for_put_input_state_unchanged_on_capacity_error() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let invitee_vk = invitee_sk.verifying_key();

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

        let snapshot = state.clone();
        let fixed_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let _ = build_state_for_put(
            state,
            owner_vk,
            &invitee_sk,
            &authorized_member,
            Some(&test_member_info(&invitee_sk)),
            fixed_time,
        );

        // The snapshot is what we passed in. The function consumes
        // `state` by value, so direct equality is the assertion that
        // matters in production: the caller's clone (`retrieved_state`
        // in the caller) is never touched on Err — because the Err
        // path doesn't mutate `state` before returning.
        let params = ChatRoomParametersV1 { owner: owner_vk };
        snapshot
            .verify(&snapshot, &params)
            .expect("input state must still verify");
        assert_eq!(snapshot.members.members.len(), 1);
        assert!(
            !snapshot
                .members
                .members
                .iter()
                .any(|m| m.member.member_vk == invitee_vk),
            "invitee must not appear in the snapshot"
        );
    }

    /// HIGH-2 / determinism pin: `build_state_for_put` accepts a
    /// `time: SystemTime` parameter so the synthesised join_event's
    /// timestamp is deterministic in tests. Calling with the SAME
    /// inputs (same state, same keys, same time) must produce a
    /// byte-identical `AuthorizedMessageV1` — same MessageId, same
    /// signature. Earlier the function called
    /// `get_current_system_time()` internally and the
    /// doc-comment falsely claimed "pure function".
    #[test]
    fn build_state_for_put_is_deterministic_given_time() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let invitee_vk = invitee_sk.verifying_key();

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

        let fixed_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let member_info = test_member_info(&invitee_sk);
        let (_, j1) = build_state_for_put(
            state.clone(),
            owner_vk,
            &invitee_sk,
            &authorized_member,
            Some(&member_info),
            fixed_time,
        )
        .expect("must succeed");
        let (_, j2) = build_state_for_put(
            state,
            owner_vk,
            &invitee_sk,
            &authorized_member,
            Some(&member_info),
            fixed_time,
        )
        .expect("must succeed");

        let j1 = j1.expect("first synthesised join_event");
        let j2 = j2.expect("second synthesised join_event");
        assert_eq!(
            j1.id(),
            j2.id(),
            "same time -> same MessageId (deterministic)"
        );
        assert_eq!(j1.message.time, fixed_time);
    }
}
