#![allow(dead_code)]

use super::error::SynchronizerError;
use crate::components::app::chat_delegate::save_rooms_to_delegate;
use crate::components::app::document_title::{
    mark_current_room_as_read, update_document_title, DOCUMENT_VISIBLE,
};
use crate::components::app::freenet_api::constants::INVITATION_TIMEOUT_MS;
use crate::components::app::notifications::{
    mark_initial_sync_complete, notify_new_messages, INITIAL_SYNC_COMPLETE,
};
use crate::components::app::receive_times::record_receive_times;
use crate::components::app::sync_info::{now_ms, RoomSyncStatus, SYNC_INFO};
use crate::components::app::{CURRENT_ROOM, PENDING_INVITES, ROOMS, WEB_API};
use crate::constants::ROOM_CONTRACT_WASM;
use crate::invites::PendingRoomStatus;
use crate::util::ecies::decrypt_with_symmetric_key;
use crate::util::{owner_vk_to_contract_key, to_cbor_vec};
use dioxus::logger::tracing::{debug, error, info, warn};
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
use river_core::room_state::member_info::MemberInfoV1;
use river_core::room_state::message::{AuthorizedMessageV1, MessageId, RoomMessageBody};
use river_core::room_state::privacy::PrivacyMode;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::collections::HashMap;
use std::sync::Arc;

fn compute_update_data(
    state: &ChatRoomStateV1,
    baseline: Option<&ChatRoomStateV1>,
    params: &ChatRoomParametersV1,
) -> Option<UpdateData<'static>> {
    if let Some(baseline) = baseline {
        let summary = baseline.summarize(baseline, params);
        let delta = state.delta(baseline, params, &summary)?;
        Some(UpdateData::Delta(to_cbor_vec(&delta).into()))
    } else {
        Some(UpdateData::State(to_cbor_vec(state).into()))
    }
}

/// Identifies contracts that have changed in order to send state updates to Freene
#[derive(Clone)]
pub struct RoomSynchronizer {
    contract_sync_info: HashMap<ContractInstanceId, ContractSyncInfo>,
}

impl RoomSynchronizer {
    /// Applies a delta update to a room's state.
    ///
    /// Like update_room_state, deferred via setTimeout(0) on WASM to prevent
    /// re-entrant signal borrow issues. See update_room_state docs for details.
    pub(crate) fn apply_delta(&self, owner_vk: &VerifyingKey, delta: ChatRoomStateV1Delta) {
        let owner_vk = *owner_vk;
        crate::util::defer(move || {
            Self::apply_delta_inner(owner_vk, delta);
        });
    }

    /// Inner implementation of apply_delta, runs in a clean execution context on WASM.
    fn apply_delta_inner(owner_vk: VerifyingKey, delta: ChatRoomStateV1Delta) {
        // Extract new messages for notifications before entering the mutable borrow
        let new_messages = delta.recent_messages.clone();
        // Will be populated INSIDE with_mut after the merge lands so it
        // contains only DMs that actually crossed the dedupe gate
        // (`AuthorizedDirectMessage` sender_signature comparison in
        // `direct_messages::apply_delta`). Issue freenet/river#267:
        // when the user hides a thread and a new inbound DM lands in
        // the same unix-second (or with clock skew putting its
        // timestamp at `hidden_at_ts`), the filter's strict-`<=` rule
        // keeps the thread hidden. Explicit unhide is deterministic
        // and idempotent — it pairs with the existing outbound-send
        // unhide in `dm_thread_modal::do_send` so both directions
        // revive symmetrically.
        //
        // Computing this AFTER the merge (not from raw `delta.direct_messages`)
        // is load-bearing: the raw delta can carry re-deliveries of
        // already-known DMs from a peer state-summary mismatch, and
        // `apply_delta` silently drops those. If we'd fired unhide on
        // every raw entry we'd un-archive a thread the user just hid
        // every time the network re-synced an already-seen DM. By
        // diffing the post-merge `direct_messages.messages` list
        // against a pre-merge signature snapshot, we only unhide for
        // DMs that genuinely just landed.
        let mut newly_landed_inbound_senders: Vec<MemberId> = Vec::new();

        // Will be populated inside with_mut if new messages need notification
        let mut pending_notification: Option<(
            Vec<_>,
            MemberId,
            MemberInfoV1,
            HashMap<u32, [u8; 32]>,
        )> = None;

        ROOMS.with_mut(|rooms| {
            if let Some(room_data) = rooms.map.get_mut(&owner_vk) {
                let params = ChatRoomParametersV1 { owner: owner_vk };

                // Log the delta being applied, especially any member_info with versions
                if let Some(member_info) = &delta.member_info {
                    debug!("Applying member_info delta with {} items", member_info.len());
                    for info in member_info {
                        debug!("Delta contains member_info with version: {} for member: {:?}, nickname: {}",
                              info.member_info.version,
                              info.member_info.member_id,
                              info.member_info.preferred_nickname);
                    }
                }

                // Log current versions before applying delta
                debug!("Current member_info state before delta ({} items):",
                      room_data.room_state.member_info.member_info.len());
                for info in &room_data.room_state.member_info.member_info {
                    debug!("Current member_info version: {} for member: {:?}, nickname: {}",
                          info.member_info.version,
                          info.member_info.member_id,
                          info.member_info.preferred_nickname);
                }

                // Capture data for notifications before we modify room_data.
                // self_member_id is independent of room_state so it's fine to
                // snapshot pre-merge. room_secrets is captured AFTER the merge
                // + repopulate below — see #251 / Codex P3: a delta carrying a
                // back-filled secret AND new private messages would otherwise
                // leave the notification path using the pre-merge (empty) map
                // and rendering encrypted placeholders in the preview.
                let self_member_id: MemberId = room_data.self_sk.verifying_key().into();

                // Issue freenet/river#267: snapshot pre-merge DM
                // signatures so we can compute "what actually landed"
                // post-merge. The raw `delta.direct_messages` may
                // include re-deliveries that the contract dedupe
                // silently drops — we must NOT unhide for those.
                let pre_merge_dm_sigs: std::collections::HashSet<[u8; 64]> = room_data
                    .room_state
                    .direct_messages
                    .messages
                    .iter()
                    .map(|m| m.sender_signature.to_bytes())
                    .collect();

                // The `parent_state` arg to `apply_delta` is dead-code at the
                // top level: the macro-generated `apply_delta` for
                // `ChatRoomStateV1` ignores its outer `_parent_state` and uses
                // a freshly-cloned `self_clone` *per field* as each field's
                // baseline (see `freenet-scaffold-macro` 0.2.2). All
                // field-level `summarize` / `delta` impls also take
                // `_parent_state` (unused). Passing a cheap default sentinel
                // here is provably equivalent to the previous
                // `room_data.room_state.clone()` and saves one full-state
                // clone per network delta — freenet/river#246 follow-up.
                let parent_sentinel = ChatRoomStateV1::default();

                match room_data
                    .room_state
                    .apply_delta(&parent_sentinel, &params, &Some(delta))
                {
                    Ok(_) => {
                        // For private rooms, rebuild actions_state with decrypted content
                        // (apply_delta only processes public actions)
                        let is_private = room_data.room_state.configuration.configuration.privacy_mode
                            == PrivacyMode::Private;
                        if is_private {
                            // #251: bring `room_data.secrets` up to date with any
                            // encrypted blobs that the delta carried in for us
                            // (e.g. the delegate's PR #245 back-fill on join, or
                            // a rotation). Must run BEFORE the action_state
                            // rebuild below, which reads `get_secret_for_version`.
                            let new_secrets = room_data.repopulate_secrets_from_state();
                            if new_secrets > 0 {
                                debug!(
                                    "apply_delta: decrypted {} new room secret(s) for {:?}",
                                    new_secrets,
                                    MemberId::from(owner_vk)
                                );
                            }

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
                        debug!("Updated member_info state after delta ({} items):",
                              room_data.room_state.member_info.member_info.len());
                        for info in &room_data.room_state.member_info.member_info {
                            debug!("Updated member_info version: {} for member: {:?}, nickname: {}",
                                  info.member_info.version,
                                  info.member_info.member_id,
                                  info.member_info.preferred_nickname);
                        }

                        // Keep cached self membership data up to date
                        room_data.capture_self_membership_data(&params);

                        // Issue freenet/river#267: compute newly-landed
                        // INBOUND DM senders by diffing the post-merge
                        // signature set against the pre-merge snapshot.
                        // Filtering to recipient == self_member_id
                        // ensures we don't unhide for outbound DMs (which
                        // already get their own unhide in the send path)
                        // or for DMs between two other members (which
                        // wouldn't be in a hidden thread of ours anyway).
                        for msg in &room_data.room_state.direct_messages.messages {
                            let sig_bytes = msg.sender_signature.to_bytes();
                            if pre_merge_dm_sigs.contains(&sig_bytes) {
                                continue;
                            }
                            if msg.message.recipient != self_member_id {
                                continue;
                            }
                            if msg.message.sender == self_member_id {
                                // Self-DM is dropped by the contract,
                                // but defence-in-depth.
                                continue;
                            }
                            newly_landed_inbound_senders.push(msg.message.sender);
                        }

                        // NOTE: We do not update last_synced_state in the delta path.
                        // We only have a delta (not the full contract state), so we can't
                        // set the baseline to the contract's actual state. The full-state path
                        // (update_room_state_inner) handles baseline updates correctly.
                        // This may cause one redundant UPDATE on the next sync cycle, but
                        // it's harmless since the contract will see it as a no-op merge.

                        // Store notification data for AFTER with_mut completes
                        // (notify_new_messages calls ROOMS.read() internally, causing deadlock if called here)
                        if let Some(messages) = new_messages {
                            // Record receive timestamps for propagation delay tracking
                            let msg_ids: Vec<_> = messages.iter().map(|m| m.id()).collect();
                            record_receive_times(&msg_ids);

                            let updated_member_info = room_data.room_state.member_info.clone();
                            // Capture secrets AFTER repopulate so the
                            // notification preview can decrypt private messages
                            // encrypted at a version that was back-filled in
                            // this same delta. See #251 / Codex P3.
                            let room_secrets = room_data.secrets.clone();
                            pending_notification = Some((messages, self_member_id, updated_member_info, room_secrets));
                        }

                        // Persist to delegate so state survives refresh
                        crate::platform::spawn_local(async {
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
            }
        });

        // Issue freenet/river#267: revive any hidden thread for an
        // (owner_vk, sender) pair that just received an inbound DM.
        // `newly_landed_inbound_senders` was populated INSIDE with_mut
        // after the merge gate, so it contains only DMs that actually
        // crossed the dedupe (raw `delta.direct_messages.new_messages`
        // can carry re-deliveries the contract silently drops — see
        // the pre-merge-signature-snapshot comment above for why we
        // can't trust the raw delta here). Outbound DMs go through
        // their own `unhide_dm_thread` call site in
        // `dm_thread_modal::do_send` / `direct_messages::send_structured_dm`,
        // so we don't need a self-id filter on this path.
        if !newly_landed_inbound_senders.is_empty() {
            // De-duplicate before firing the unhide (multiple inbound
            // DMs from the same peer in one batch only need one unhide
            // call). `unhide_dm_thread` is idempotent, so duplicates
            // are safe, but de-duping avoids redundant delegate saves.
            let mut seen: std::collections::HashSet<MemberId> = std::collections::HashSet::new();
            for sender in newly_landed_inbound_senders {
                if seen.insert(sender) {
                    crate::components::app::chat_delegate::unhide_dm_thread(owner_vk, sender);
                }
            }
        }

        // Update document title after ROOMS.with_mut completes (update_document_title calls ROOMS.read())
        update_document_title();

        // Now safe to call notify_new_messages (it calls ROOMS.read() internally)
        if let Some((messages, self_member_id, member_info, room_secrets)) = pending_notification {
            notify_new_messages(
                &owner_vk,
                &messages,
                self_member_id,
                &member_info,
                &room_secrets,
            );

            // If user is viewing this room with tab visible, mark as read
            let is_visible = *DOCUMENT_VISIBLE.read();
            let is_current_room = CURRENT_ROOM.read().owner_key == Some(owner_vk);
            if is_visible && is_current_room {
                mark_current_room_as_read();
            }
        }
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
                        join.retry_count += 1;
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
                // and read the retry count to decide whether to request contract code
                let retry_count = PENDING_INVITES.with_mut(|pending| {
                    if let Some(join) = pending.map.get_mut(&owner_vk) {
                        join.status = PendingRoomStatus::Subscribing;
                        join.subscribing_since = Some(now_ms());
                        join.retry_count
                    } else {
                        0
                    }
                });

                // Always request contract code so the node caches the WASM locally.
                // Without cached WASM, subsequent Subscribe requests will be rejected
                // by the node (freenet-core#3601).
                let request_code = true;
                if retry_count >= 1 {
                    warn!("Retry #{} for {:?}", retry_count, MemberId::from(owner_vk));
                }

                let get_request = ContractRequest::Get {
                    key: *contract_key.id(),
                    return_contract_code: request_code,
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
                let contract_key = owner_vk_to_contract_key(owner_vk);
                let contract_id = contract_key.id();

                // Imported rooms have default state with an invalid configuration
                // signature (only the owner can sign it). GET the real state first,
                // then the GET response handler will PUT+subscribe with valid state.
                let needs_get_first = ROOMS
                    .read()
                    .map
                    .get(owner_vk)
                    .is_some_and(|rd| rd.is_awaiting_initial_sync());

                if needs_get_first {
                    // Imported room with default state — GET the real state from the
                    // network first. PUTting the default state would fail because its
                    // configuration signature is invalid (only the owner can sign it).
                    // The GET response handler will merge the retrieved state and then
                    // PUT+subscribe with valid state.
                    info!(
                        "Room {:?} has default state (import), sending GET instead of PUT",
                        MemberId::from(*owner_vk)
                    );

                    SYNC_INFO.with_mut(|sync_info| {
                        sync_info.register_new_room(*owner_vk);
                    });

                    let get_request = ContractRequest::Get {
                        key: *contract_id,
                        return_contract_code: true,
                        subscribe: false,
                        blocking_subscribe: false,
                    };

                    if let Some(web_api) = WEB_API.write().as_mut() {
                        match web_api.send(ClientRequest::ContractOp(get_request)).await {
                            Ok(_) => {
                                info!("Sent GET for imported room {:?}", MemberId::from(*owner_vk));
                                SYNC_INFO.with_mut(|sync_info| {
                                    sync_info
                                        .update_sync_status(owner_vk, RoomSyncStatus::Subscribing);
                                });
                            }
                            Err(e) => {
                                error!(
                                    "Failed to send GET for imported room {:?}: {}",
                                    MemberId::from(*owner_vk),
                                    e
                                );
                                SYNC_INFO.with_mut(|sync_info| {
                                    sync_info.update_sync_status(
                                        owner_vk,
                                        RoomSyncStatus::Error(e.to_string()),
                                    );
                                });
                            }
                        }
                    }
                    continue;
                }

                info!("Subscribing to room: {:?}", MemberId::from(*owner_vk));

                let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
                let parameters = ChatRoomParametersV1 { owner: *owner_vk };
                let params_bytes = to_cbor_vec(&parameters);
                let parameters = Parameters::from(params_bytes);

                let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
                    WrappedContract::new(Arc::new(contract_code), parameters),
                ));

                let wrapped_state = WrappedState::new(to_cbor_vec(state));

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

        // Handle migrated rooms (freenet/river#292, Task 2).
        //
        // Previously this block force-PUT the device's *local* `room_state`
        // snapshot onto the new contract key. That re-introduced stale
        // state — old member IDs, pruned members — whenever the new key
        // already carried fresher state from the network. Instead we now
        // route the migrated room through the normal GET+subscribe path:
        // GET the new contract key, and let `handle_get_response` CRDT-
        // merge whatever the network has. If the new key turns out to be
        // empty, `handle_get_response` itself triggers the backward
        // probe (Task 3), which recovers the room's last-active state
        // from an older generation and only seeds the new key with the
        // local snapshot as a genuine last resort.
        //
        // The owner still sends the `OptionalUpgradeV1` pointer on the
        // OLD contract so old clients can find the new key.
        if web_api_available {
            let migrated_rooms: Vec<(VerifyingKey, freenet_stdlib::prelude::ContractKey)> =
                ROOMS.with_mut(|rooms| std::mem::take(&mut rooms.migrated_rooms));

            for (owner_vk, old_contract_key) in &migrated_rooms {
                let (new_contract_key, is_owner) = {
                    let rooms = ROOMS.read();
                    if let Some(room_data) = rooms.map.get(owner_vk) {
                        let is_owner = room_data.self_sk.verifying_key() == *owner_vk;
                        (room_data.contract_key, is_owner)
                    } else {
                        continue;
                    }
                };

                info!(
                    "Migrating room {:?} from old contract {} to new contract {} \
                     (GET+subscribe — network state is authoritative)",
                    MemberId::from(*owner_vk),
                    old_contract_key.id(),
                    new_contract_key.id()
                );

                // Register the new contract id so the GET response
                // resolves back to this owner.
                SYNC_INFO.with_mut(|sync_info| {
                    sync_info.register_new_room(*owner_vk);
                });

                // Any client: GET+subscribe the new contract key. The
                // GET response handler merges network state and, when
                // the new key is empty, fans out to the backward probe.
                let get_request = ContractRequest::Get {
                    key: *new_contract_key.id(),
                    return_contract_code: true,
                    subscribe: true,
                    blocking_subscribe: false,
                };

                if let Some(web_api) = WEB_API.write().as_mut() {
                    match web_api.send(ClientRequest::ContractOp(get_request)).await {
                        Ok(_) => {
                            info!(
                                "Sent GET+subscribe to new contract for migrated room {:?}",
                                MemberId::from(*owner_vk)
                            );
                            SYNC_INFO.with_mut(|sync_info| {
                                sync_info.update_sync_status(owner_vk, RoomSyncStatus::Subscribing);
                            });
                        }
                        Err(e) => {
                            warn!(
                                "Failed to GET new contract for migrated room {:?}: {}",
                                MemberId::from(*owner_vk),
                                e
                            );
                        }
                    }
                }

                // Owner only: send upgrade pointer to old contract for old-client compat.
                // `is_owner` guarantees `room_data.self_sk` is the owner key, so the
                // `AuthorizedUpgradeV1` signature validates against the old contract's
                // `parameters.owner`.
                if is_owner {
                    use river_core::room_state::upgrade::{AuthorizedUpgradeV1, UpgradeV1};

                    let upgrade_delta = {
                        let rooms = ROOMS.read();
                        if let Some(room_data) = rooms.map.get(owner_vk) {
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

                            // Send a minimal delta carrying ONLY the upgrade
                            // pointer — not a full `UpdateData::State`. A full
                            // state UPDATE is run through the old contract's
                            // `validate_state` -> `ChatRoomStateV1::verify`; the
                            // previous `..Default::default()` state failed that
                            // with "Invalid signature" because its default
                            // `configuration` is unsigned (issue #127). A delta
                            // is applied via `apply_delta`, which validates only
                            // the upgrade signature against the contract's owner
                            // parameter — so the payload is just the signed
                            // upgrade pointer (~100 bytes), no unsigned default
                            // `configuration` is ever substituted, and
                            // full-state verification is never tripped.
                            ChatRoomStateV1Delta {
                                upgrade: Some(authorized_upgrade),
                                ..Default::default()
                            }
                        } else {
                            continue;
                        }
                    };

                    let update_request = ContractRequest::Update {
                        key: *old_contract_key,
                        data: UpdateData::Delta(to_cbor_vec(&upgrade_delta).into()),
                    };

                    if let Some(web_api) = WEB_API.write().as_mut() {
                        match web_api
                            .send(ClientRequest::ContractOp(update_request))
                            .await
                        {
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
        }

        info!("Checking for rooms to update");

        // Only check for rooms needing updates if WebAPI is available
        let rooms_to_sync = if web_api_available {
            SYNC_INFO.with_mut(|sync_info| sync_info.needs_to_send_update())
        } else {
            std::collections::HashMap::new()
        };

        crate::util::debug_log(&format!("[sync] {} rooms need sync", rooms_to_sync.len()));
        info!(
            "Found {} rooms that need synchronization",
            rooms_to_sync.len()
        );

        for (room_vk, (mut state, last_synced_state)) in rooms_to_sync {
            info!("Processing room: {:?}", MemberId::from(room_vk));

            // Sanitize: remove any messages with invalid signatures before
            // sending to the contract. This catches messages that were signed
            // by a stale delegate key (e.g., before identity import migration
            // completed) and prevents the contract from rejecting the entire
            // update due to one bad signature.
            let params = ChatRoomParametersV1 { owner: room_vk };
            let removed = crate::signing::remove_unverifiable_messages(&mut state, &params);
            if removed > 0 {
                warn!(
                    "Removed {} message(s) with invalid signatures before sync for room {:?}",
                    removed,
                    MemberId::from(room_vk)
                );
                // Persist the cleaned state back to ROOMS
                ROOMS.with_mut(|rooms| {
                    if let Some(rd) = rooms.map.get_mut(&room_vk) {
                        rd.room_state = state.clone();
                    }
                });
            }

            // If sanitization emptied the state, don't send an empty UPDATE —
            // instead, GET fresh state from the network to repopulate.
            let is_empty_after_sanitize = removed > 0
                && state.members.members.is_empty()
                && state.recent_messages.messages.is_empty();
            if is_empty_after_sanitize {
                warn!(
                    "Room {:?} state empty after sanitization, fetching fresh state via GET",
                    MemberId::from(room_vk)
                );
                // Update last_synced_state to the sanitized (empty) state so the
                // next sync cycle doesn't re-trigger sanitization before the GET
                // response arrives.
                SYNC_INFO.with_mut(|sync_info| {
                    sync_info.state_updated(&room_vk, state);
                });
                let contract_key = owner_vk_to_contract_key(&room_vk);
                let get_request = ContractRequest::Get {
                    key: *contract_key.id(),
                    return_contract_code: false,
                    subscribe: false,
                    blocking_subscribe: false,
                };
                if let Some(web_api) = WEB_API.write().as_mut() {
                    if let Err(e) = web_api.send(ClientRequest::ContractOp(get_request)).await {
                        error!(
                            "Failed to GET fresh state for room {:?}: {}",
                            MemberId::from(room_vk),
                            e
                        );
                    }
                }
                continue;
            }

            let contract_key = owner_vk_to_contract_key(&room_vk);

            let update_data = match compute_update_data(&state, last_synced_state.as_ref(), &params)
            {
                Some(data) => {
                    match &data {
                        UpdateData::Delta(d) => info!(
                            "Room {:?}: sending delta ({} bytes)",
                            MemberId::from(room_vk),
                            d.as_ref().len(),
                        ),
                        _ => info!(
                            "Room {:?}: no baseline, sending full state",
                            MemberId::from(room_vk),
                        ),
                    }
                    data
                }
                None => {
                    SYNC_INFO.with_mut(|sync_info| {
                        sync_info.state_updated(&room_vk, state);
                    });
                    continue;
                }
            };

            let update_request = ContractRequest::Update {
                key: contract_key,
                data: update_data,
            };

            let client_request = ClientRequest::ContractOp(update_request);

            if let Some(web_api) = WEB_API.write().as_mut() {
                crate::util::debug_log("[sync] sending UPDATE via WebSocket...");
                match web_api.send(client_request).await {
                    Ok(_) => {
                        crate::util::debug_log("[sync] UPDATE sent OK");
                        info!(
                            "Successfully sent update for room: {:?}",
                            MemberId::from(room_vk)
                        );
                        // Only update the last synced state after successfully sending the update
                        SYNC_INFO.with_mut(|sync_info| {
                            sync_info.state_updated(&room_vk, state.clone());
                        });
                    }
                    Err(e) => {
                        crate::util::debug_log(&format!("[sync] UPDATE FAILED: {}", e));
                        // Don't fail the entire process if one room fails
                        error!(
                            "Failed to send update for room {:?}: {}",
                            MemberId::from(room_vk),
                            e
                        );
                    }
                }
            } else {
                crate::util::debug_log("[sync] WebAPI unavailable!");
                // This shouldn't happen since we checked at the start
                warn!("WebAPI became unavailable during processing");
            }
        }

        info!("Finished processing all rooms");

        Ok(())
    }

    /// Updates the room state and last_sync_state, should be called after state update received from network.
    ///
    /// IMPORTANT: On WASM targets, the actual state mutation is deferred via setTimeout(0).
    /// This prevents re-entrant signal borrow panics: Dioxus fires subscriber notifications
    /// synchronously during Drop of the write guard, which causes `try_read()` in `use_memo`
    /// closures to fail. When `try_read()` fails, the memo doesn't subscribe to ROOMS and
    /// permanently stops re-evaluating — causing "messages not visible until you post" bugs.
    /// setTimeout(0) breaks out of the WASM call stack, ensuring the write happens in a
    /// clean execution context where no signal borrows are active.
    pub(crate) fn update_room_state(&self, room_owner_vk: &VerifyingKey, state: &ChatRoomStateV1) {
        let room_owner_vk = *room_owner_vk;
        let state = state.clone();
        crate::util::defer(move || {
            Self::update_room_state_inner(room_owner_vk, state);
        });
    }

    /// Inner implementation of update_room_state, runs in a clean execution context on WASM.
    fn update_room_state_inner(room_owner_vk: VerifyingKey, state: ChatRoomStateV1) {
        // Capture data needed for notifications BEFORE the mutable borrow.
        // room_secrets is NOT captured here — see #251 / Codex P3: a state
        // update may carry a back-filled secret AND new private messages in
        // the same payload; the pre-merge map would be stale by the time we
        // try to decrypt the new messages for the notification preview.
        // It's re-captured post-merge + post-repopulate inside `with_mut`.
        //
        // `pre_merge_dm_sigs` mirrors the apply_delta_inner snapshot for
        // issue freenet/river#267 — the full-state merge path needs the
        // same unhide-on-new-inbound-DM behaviour so a refresh GET
        // (after sleep, resubscription, etc.) doesn't leave a hidden
        // thread stuck when a fresh inbound DM lands within the
        // strict-`<=` window. Codex review finding on PR #286.
        let (old_message_ids, self_member_id, member_info_clone, pre_merge_dm_sigs) = {
            let Ok(rooms) = ROOMS.try_read() else {
                warn!("update_room_state: ROOMS is currently borrowed, skipping update");
                return;
            };
            if let Some(room_data) = rooms.map.get(&room_owner_vk) {
                let old_ids: std::collections::HashSet<_> = room_data
                    .room_state
                    .recent_messages
                    .messages
                    .iter()
                    .map(|m| m.id())
                    .collect();
                debug!(
                    "update_room_state: Captured {} old message IDs for room {:?}",
                    old_ids.len(),
                    MemberId::from(room_owner_vk)
                );
                let self_id = MemberId::from(&room_data.self_sk.verifying_key());
                let member_info = room_data.room_state.member_info.clone();
                let dm_sigs: std::collections::HashSet<[u8; 64]> = room_data
                    .room_state
                    .direct_messages
                    .messages
                    .iter()
                    .map(|m| m.sender_signature.to_bytes())
                    .collect();
                (Some(old_ids), Some(self_id), Some(member_info), dm_sigs)
            } else {
                debug!(
                    "update_room_state: Room {:?} not found in ROOMS when capturing old IDs",
                    MemberId::from(room_owner_vk)
                );
                (
                    None,
                    None,
                    None,
                    std::collections::HashSet::<[u8; 64]>::new(),
                )
            }
        };

        // Log incoming state message count
        debug!(
            "update_room_state: Incoming state has {} messages for room {:?}",
            state.recent_messages.messages.len(),
            MemberId::from(room_owner_vk)
        );

        // Will be populated inside with_mut if new messages are detected.
        // Tuple: (new_messages, self_member_id, room_secrets_post_repopulate).
        // room_secrets travels with the notification so the preview can
        // decrypt messages encrypted at a version back-filled in this same
        // update. See #251 / Codex P3.
        type PendingNotification = (Vec<AuthorizedMessageV1>, MemberId, HashMap<u32, [u8; 32]>);
        let mut pending_notification: Option<PendingNotification> = None;
        // Updated member_info captured after state merge (so new sender nicknames are included)
        let mut updated_member_info: Option<MemberInfoV1> = None;
        // Issue freenet/river#267 (full-state path): post-merge inbound
        // DM senders for hidden-thread revival. Same shape as the
        // delta-path local in apply_delta_inner.
        let mut newly_landed_inbound_senders: Vec<MemberId> = Vec::new();
        let room_owner_copy = room_owner_vk;

        ROOMS.with_mut(|rooms| {
            if let Some(room_data) = rooms.map.get_mut(&room_owner_vk) {
                // Log member info versions before merge
                debug!(
                    "Before merge - Local member info versions ({} items):",
                    room_data.room_state.member_info.member_info.len()
                );
                for info in &room_data.room_state.member_info.member_info {
                    debug!(
                        "  Member: {:?}, Version: {}, Nickname: {}",
                        info.member_info.member_id,
                        info.member_info.version,
                        info.member_info.preferred_nickname
                    );
                }

                debug!(
                    "Before merge - Incoming state member info versions ({} items):",
                    state.member_info.member_info.len()
                );
                for info in &state.member_info.member_info {
                    debug!(
                        "  Member: {:?}, Version: {}, Nickname: {}",
                        info.member_info.member_id,
                        info.member_info.version,
                        info.member_info.preferred_nickname
                    );
                }

                // Update the room state by merging the new state with the
                // existing one. The `parent_state` arg is dead-code at the
                // macro/trait level for the `merge` call too — but the proof
                // shape is slightly different from `apply_delta_inner` and
                // worth spelling out: the default `ComposableState::merge` in
                // `freenet-scaffold` calls `self.summarize(parent_state,...)`,
                // `other.delta(parent_state,...)`, and `self.apply_delta(parent_state,...)`.
                // The macro-generated `summarize`/`delta` forward `parent_state`
                // to each field's impl, so safety here ALSO depends on every
                // field-level `summarize`/`delta` in `common/src/room_state/`
                // declaring the arg as `_parent_state` (unused) — verified
                // across all 9 fields. The `apply_delta` leg is protected by
                // the macro's per-field `self.clone()` as documented on the
                // `apply_delta_inner` call site above. We pass a cheap default
                // sentinel rather than cloning the full `room_data.room_state`;
                // saves one full-state clone per network state-update event.
                // Together with the equivalent change on the `apply_delta`
                // path, this chips at the per-event allocation cost that
                // survived the initial `coalesce_save` fix for
                // freenet/river#246. The regression test
                // `merge_with_default_sentinel_parent_matches_merge_with_self_clone_parent`
                // pins this invariant against future macro / field-impl
                // refactors that would break the substitution.
                let parent_sentinel = ChatRoomStateV1::default();
                match room_data.room_state.merge(
                    &parent_sentinel,
                    &ChatRoomParametersV1 {
                        owner: room_owner_vk,
                    },
                    &state,
                ) {
                    Ok(_) => {
                        // For private rooms, rebuild actions_state with decrypted content
                        let is_private = room_data.room_state.configuration.configuration.privacy_mode
                            == PrivacyMode::Private;
                        if is_private {
                            // #251: bring `room_data.secrets` up to date with any
                            // encrypted blobs that this state update carried in
                            // for us (e.g. the delegate's PR #245 back-fill on
                            // join, or a rotation). Must run BEFORE the
                            // action_state rebuild below, which reads
                            // `get_secret_for_version`.
                            let new_secrets = room_data.repopulate_secrets_from_state();
                            if new_secrets > 0 {
                                debug!(
                                    "update_room_state: decrypted {} new room secret(s) for {:?}",
                                    new_secrets,
                                    MemberId::from(room_owner_vk)
                                );
                            }

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
                        debug!(
                            "After merge - Updated member info versions ({} items):",
                            room_data.room_state.member_info.member_info.len()
                        );
                        for info in &room_data.room_state.member_info.member_info {
                            debug!(
                                "  Member: {:?}, Version: {}, Nickname: {}",
                                info.member_info.member_id,
                                info.member_info.version,
                                info.member_info.preferred_nickname
                            );
                        }

                        // Keep cached self membership data up to date
                        let params = ChatRoomParametersV1 { owner: room_owner_vk };
                        room_data.capture_self_membership_data(&params);

                        // Issue freenet/river#267 (full-state path):
                        // diff post-merge DM signatures against the
                        // pre-merge snapshot to find genuinely new
                        // inbound DMs and queue an unhide for each
                        // sender. Mirrors the apply_delta_inner path
                        // exactly. Codex review finding on PR #286 —
                        // without this, a hidden thread that receives
                        // a new inbound DM via a refresh GET (after
                        // sleep / resubscription) stays archived even
                        // though a new message arrived.
                        let self_id_for_unhide = self_member_id;
                        if let Some(self_id) = self_id_for_unhide {
                            for msg in &room_data.room_state.direct_messages.messages {
                                let sig_bytes = msg.sender_signature.to_bytes();
                                if pre_merge_dm_sigs.contains(&sig_bytes) {
                                    continue;
                                }
                                if msg.message.recipient != self_id {
                                    continue;
                                }
                                if msg.message.sender == self_id {
                                    continue;
                                }
                                newly_landed_inbound_senders.push(msg.message.sender);
                            }
                        }

                        // Make sure the room is registered in SYNC_INFO and update the
                        // baseline to the INCOMING contract state (not the post-merge state).
                        // The incoming state represents what the contract currently has.
                        // If we used the post-merge state (which includes any pending local
                        // changes), needs_to_send_update() would see states_match==true and
                        // skip sending the user's pending changes. By using the incoming state,
                        // local changes remain as a detectable diff above the baseline.
                        SYNC_INFO.with_mut(|sync_info| {
                            sync_info.register_new_room(room_owner_vk);
                            sync_info.update_last_synced_state(&room_owner_vk, &state);
                        });

                        // Check if initial sync was already complete before this update
                        let was_sync_complete = INITIAL_SYNC_COMPLETE.read().contains(&room_owner_vk);

                        // Mark initial sync complete for this room (enables notifications)
                        mark_initial_sync_complete(&room_owner_vk);

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
                                    MemberId::from(room_owner_vk)
                                );

                                // Only record receive times after initial sync — during
                                // initial load, messages may have arrived long ago
                                if was_sync_complete {
                                    let new_msg_ids: Vec<_> = new_messages.iter().map(|m| m.id()).collect();
                                    record_receive_times(&new_msg_ids);
                                }

                                // Store for notification after with_mut completes
                                // Capture member_info from the UPDATED state so new sender nicknames are included.
                                // Capture room_secrets AFTER the merge + repopulate
                                // above so the notification preview can decrypt
                                // messages whose secret was back-filled in this
                                // update. See #251 / Codex P3.
                                updated_member_info = Some(room_data.room_state.member_info.clone());
                                let room_secrets = room_data.secrets.clone();
                                pending_notification = Some((new_messages, self_id, room_secrets));
                            } else {
                                info!(
                                    "No new messages detected for room {:?} (old_ids: {}, post-merge: {})",
                                    MemberId::from(room_owner_vk),
                                    old_ids.len(),
                                    room_data.room_state.recent_messages.messages.len()
                                );
                            }
                        }

                        // Persist to delegate so state survives refresh
                        crate::platform::spawn_local(async {
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

                // Register the room in SYNC_INFO to trigger a GET request
                SYNC_INFO.with_mut(|sync_info| {
                    sync_info.register_new_room(room_owner_vk);
                });

                info!("Registered room {:?} for GET request after receiving update without existing room data", MemberId::from(room_owner_vk));
            }
        });

        // Issue freenet/river#267 (full-state path): unhide any thread
        // whose post-merge DM set gained a new inbound DM from the
        // peer. Symmetric with the apply_delta_inner path. Codex
        // review finding on PR #286.
        if !newly_landed_inbound_senders.is_empty() {
            let mut seen: std::collections::HashSet<MemberId> = std::collections::HashSet::new();
            for sender in newly_landed_inbound_senders {
                if seen.insert(sender) {
                    crate::components::app::chat_delegate::unhide_dm_thread(room_owner_vk, sender);
                }
            }
        }

        // Update document title after ROOMS.with_mut completes (update_document_title calls ROOMS.read())
        update_document_title();

        // Now safe to call notify_new_messages (it calls ROOMS.read() internally)
        // Use updated_member_info (captured after state merge) so new sender nicknames are included.
        // room_secrets travels in `pending_notification` so it reflects the
        // post-repopulate state (see #251 / Codex P3).
        if let (Some((new_messages, self_id, room_secrets)), Some(member_info)) = (
            pending_notification,
            updated_member_info.or(member_info_clone),
        ) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
    use std::time::SystemTime;

    fn create_test_room() -> (ChatRoomStateV1, ChatRoomParametersV1, SigningKey) {
        let owner_sk = SigningKey::generate(&mut rand::thread_rng());
        let owner_vk = owner_sk.verifying_key();
        let params = ChatRoomParametersV1 { owner: owner_vk };
        let state = ChatRoomStateV1::default();
        (state, params, owner_sk)
    }

    fn add_message(state: &mut ChatRoomStateV1, author_sk: &SigningKey, content: &str) {
        let msg = MessageV1 {
            room_owner: state.configuration.configuration.owner_member_id,
            author: MemberId::from(&author_sk.verifying_key()),
            content: RoomMessageBody::public(content.to_string()),
            time: SystemTime::now(),
        };
        let authorized = AuthorizedMessageV1::new(msg, author_sk);
        state.recent_messages.messages.push(authorized);
    }

    #[test]
    fn no_baseline_returns_full_state() {
        let (state, params, _) = create_test_room();
        let result = compute_update_data(&state, None, &params);
        assert!(matches!(result, Some(UpdateData::State(_))));
    }

    #[test]
    fn identical_states_returns_none() {
        let (state, params, _) = create_test_room();
        let result = compute_update_data(&state, Some(&state), &params);
        assert!(result.is_none());
    }

    #[test]
    fn changed_state_returns_delta() {
        let (state, params, owner_sk) = create_test_room();
        let baseline = state.clone();

        let mut current = state;
        add_message(&mut current, &owner_sk, "hello");

        let result = compute_update_data(&current, Some(&baseline), &params);
        assert!(matches!(result, Some(UpdateData::Delta(_))));
    }

    #[test]
    fn delta_is_smaller_than_full_state() {
        let (mut state, params, owner_sk) = create_test_room();
        for i in 0..10 {
            add_message(&mut state, &owner_sk, &format!("message {}", i));
        }
        let baseline = state.clone();

        let mut current = state;
        add_message(&mut current, &owner_sk, "new message");

        let delta = compute_update_data(&current, Some(&baseline), &params).unwrap();
        let full = compute_update_data(&current, None, &params).unwrap();

        let delta_size = match &delta {
            UpdateData::Delta(d) => d.as_ref().len(),
            _ => panic!("expected delta"),
        };
        let full_size = match &full {
            UpdateData::State(s) => s.as_ref().len(),
            _ => panic!("expected state"),
        };

        assert!(
            delta_size < full_size,
            "delta ({} bytes) should be smaller than full state ({} bytes)",
            delta_size,
            full_size
        );
    }

    // -----------------------------------------------------------------
    // Issue freenet/river#267 regression guard:
    //
    // The DM rail filter uses strict-`<=` against `hidden_at_ts` (see
    // `chat_delegate::is_thread_hidden`), so an inbound DM whose
    // timestamp falls exactly on the cutoff (same unix-second as the
    // hide, or clock skew) leaves the thread hidden. The fix is an
    // explicit `unhide_dm_thread(owner_vk, sender)` call from the
    // inbound delta path in `apply_delta_inner`, mirroring the
    // outbound-send unhide in `dm_thread_modal::do_send` and
    // `direct_messages::send_structured_dm`.
    //
    // We can't unit-test the full delta path without standing up the
    // Dioxus runtime + ROOMS signal, so this is a source-text pin:
    // the wiring MUST extract inbound senders from the delta and feed
    // them into `unhide_dm_thread`. A future refactor that drops the
    // call site would otherwise silently re-regress #267.
    // -----------------------------------------------------------------
    #[test]
    fn apply_delta_inner_revives_hidden_thread_for_inbound_dm_sender() {
        let src = include_str!("room_synchronizer.rs");
        // The unhide MUST be computed from the post-merge signature
        // diff, NOT from the raw delta. The raw delta can carry
        // re-deliveries that the contract silently drops; firing
        // unhide on those would un-archive a thread the user just hid
        // every time the network re-synced.
        assert!(
            src.contains("newly_landed_inbound_senders"),
            "apply_delta_inner must collect newly-landed (post-merge) inbound \
             DM senders, not raw delta entries, so re-deliveries don't \
             spuriously un-archive a freshly-hidden thread (#267)."
        );
        assert!(
            src.contains("pre_merge_dm_sigs"),
            "apply_delta_inner must snapshot pre-merge DM signatures so it \
             can diff against the post-merge set to find genuinely new DMs (#267)."
        );
        assert!(
            src.contains("chat_delegate::unhide_dm_thread("),
            "apply_delta_inner must call unhide_dm_thread on each newly-landed \
             inbound DM sender so a hidden thread is revived even when the new \
             DM's timestamp matches the hide cutoff exactly (#267). The filter's \
             strict-`<=` rule alone is not sufficient for the same-second case."
        );
    }

    /// Codex review finding on PR #286: the delta-path unhide alone
    /// leaves the same bug reachable when DMs arrive via the
    /// full-state merge path (refresh GET after sleep / resubscription
    /// / initial sync). The `update_room_state_inner` path must
    /// apply the same diff-and-unhide logic.
    #[test]
    fn update_room_state_inner_also_revives_hidden_thread_for_inbound_dm() {
        let src = include_str!("room_synchronizer.rs");
        // Find the update_room_state_inner function body and assert
        // the pre-merge snapshot + post-merge collection + unhide
        // call all appear AFTER its declaration. We don't try to
        // parse Rust; instead we split the file at the function
        // signature and look at the suffix.
        let marker = "fn update_room_state_inner(";
        let split_at = src.find(marker).expect(
            "update_room_state_inner must exist in this file — the test is targeting the wrong path",
        );
        let suffix = &src[split_at..];
        // The same shape as the apply_delta_inner pins, but on the
        // suffix slice so we know they're in this function.
        assert!(
            suffix.contains("pre_merge_dm_sigs"),
            "update_room_state_inner must snapshot pre-merge DM signatures \
             so full-state-path DM arrivals revive a hidden thread (#267)."
        );
        assert!(
            suffix.contains("newly_landed_inbound_senders"),
            "update_room_state_inner must collect newly-landed inbound DM \
             senders post-merge (#267)."
        );
        assert!(
            suffix.contains("chat_delegate::unhide_dm_thread("),
            "update_room_state_inner must call unhide_dm_thread on each \
             newly-landed inbound DM sender so the #267 fix covers both the \
             delta path AND the full-state merge path. Without this, a \
             refresh GET that delivers a new inbound DM into a hidden thread \
             leaves the thread archived."
        );
    }

    /// Pin that `merge` with a default-sentinel `parent_state` is
    /// byte-equivalent to `merge` with `&self.clone()` as `parent_state`,
    /// for the realistic shapes `update_room_state_inner` actually hands
    /// to `merge` (existing room state + incoming network state).
    ///
    /// This is the regression test for the per-event clone-reduction
    /// (freenet/river#246 follow-up): we replaced
    /// `room_state.merge(&room_state.clone(), &params, &incoming)` with
    /// `room_state.merge(&ChatRoomStateV1::default(), &params, &incoming)`
    /// on the assumption that `parent_state` is dead-code at the macro
    /// level. The assumption holds because every field's `summarize` /
    /// `delta` impl in `common/src/room_state/` takes `_parent_state`
    /// (unused), and the macro-generated `apply_delta` ignores its outer
    /// `_parent_state` and uses `self.clone()` per-field instead.
    ///
    /// **Discrimination design** (skeptical-review #312 caught the first
    /// cut of this test was tautological on near-default data): for the
    /// test to actually catch a future regression where the macro starts
    /// forwarding the outer `_parent_state` to a field's `apply_delta`,
    /// the two paths must hand `apply_delta` outer values that DIFFER in
    /// fields a real-world `apply_delta` impl reads. `MembersV1::apply_delta`
    /// reads `parent_state.configuration.configuration.max_members` and
    /// `parent_state.bans`; `MessagesV1::apply_delta` reads
    /// `max_recent_messages`, `max_message_size`, `privacy_mode`. So
    /// `state_a` here is set up with a non-default `max_members` AND a
    /// non-empty `bans` AND an extra non-owner member — any of which is
    /// enough to make `default()` and `state_a` produce different
    /// downstream `apply_delta` behavior IF a regression starts plumbing
    /// the outer arg through.
    #[test]
    fn merge_with_default_sentinel_parent_matches_merge_with_self_clone_parent() {
        use ed25519_dalek::SigningKey;
        use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
        use river_core::room_state::configuration::AuthorizedConfigurationV1;

        let (mut state_a, params, owner_sk) = create_test_room();

        // Make `state_a` diverge from `ChatRoomStateV1::default()` in
        // fields that real-world `apply_delta` impls actually read. Any
        // ONE of these would be enough to discriminate; we use all three
        // so the test is robust to which field-level impl a future
        // regression hits.
        //
        // 1) Non-default configuration values (`MembersV1` /
        //    `MemberInfoV1` / `MessagesV1` all read these via parent).
        let mut new_config = state_a.configuration.configuration.clone();
        new_config.max_members = 3; // default is 200
        new_config.max_recent_messages = 5; // default is 100
        state_a.configuration = AuthorizedConfigurationV1::new(new_config, &owner_sk);

        // 2) A non-owner ban — `MembersV1::apply_delta` reads
        //    `parent_state.bans` to enforce ban-sweep.
        let banned_member_sk = SigningKey::generate(&mut rand::thread_rng());
        let banned_member_id = MemberId::from(banned_member_sk.verifying_key());
        let ban = UserBan {
            owner_member_id: state_a.configuration.configuration.owner_member_id,
            banned_at: SystemTime::now(),
            banned_user: banned_member_id,
        };
        state_a.bans.0.push(AuthorizedUserBan::new(
            ban,
            MemberId::from(&owner_sk.verifying_key()),
            &owner_sk,
        ));

        // 3) A few owner-authored messages so the merge has real content
        //    to fold over (Configuration's `owner_member_id` is set on
        //    new_config above, so messages from the owner key verify).
        add_message(&mut state_a, &owner_sk, "existing-1");
        add_message(&mut state_a, &owner_sk, "existing-2");

        // `state_b` starts as a byte-equal copy of `state_a` so we can
        // compare post-merge state across the two `parent_state` shapes.
        let mut state_b = state_a.clone();

        // Incoming state: same baseline plus one more message. The merge
        // should fold that one message in.
        let mut incoming = state_a.clone();
        add_message(&mut incoming, &owner_sk, "incoming-3");

        // Path A: the old shape — clone self as `parent_state`.
        let result_a = state_a.merge(&state_a.clone(), &params, &incoming);

        // Path B: the new shape — default sentinel as `parent_state`.
        let sentinel = ChatRoomStateV1::default();
        let result_b = state_b.merge(&sentinel, &params, &incoming);

        assert_eq!(
            result_a.is_ok(),
            result_b.is_ok(),
            "merge result-status disagreed between the two parent_state shapes: \
             self-clone={:?} sentinel={:?}",
            result_a,
            result_b
        );
        assert_eq!(
            state_a, state_b,
            "merge produced different post-merge state with default-sentinel \
             parent_state vs self-clone parent_state — this means a field's \
             summarize/delta started reading parent_state, or the \
             freenet-scaffold macro started forwarding _parent_state down to \
             a field's apply_delta. The clone-reduction optimization in \
             apply_delta_inner / update_room_state_inner above is no longer \
             safe; revert it or update the macro accordingly."
        );
    }
}
