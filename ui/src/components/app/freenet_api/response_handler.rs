// `pub(crate)` so the backward-probe watchdog (freenet_api::backward_probe)
// can call `get_response::handle_probe_get_response` on timeout (#292).
pub(crate) mod get_response;
mod put_response;
mod subscribe_response;
mod update_notification;
mod update_response;

use super::error::SynchronizerError;
use super::room_synchronizer::RoomSynchronizer;
use crate::components::app::chat_delegate::{
    complete_pending_public_key_request, complete_pending_request, complete_pending_sign_request,
    complete_pending_signing_key_request, decide_legacy_migration_action,
    fire_legacy_migration_request, hydrate_hidden_dm_threads, hydrate_outbound_dms_cache,
    is_legacy_delegate_key, mark_legacy_migration_done, prune_outbound_dms_for_purges,
    save_outbound_dms_to_delegate, save_rooms_to_delegate, LegacyMigrationAction,
    OUTBOUND_DMS_STORAGE_KEY, ROOMS_STORAGE_KEY,
};
use crate::components::app::document_title::{mark_current_room_as_read, update_document_title};
use crate::components::app::notifications::mark_initial_sync_complete;
use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::room_data::CurrentRoom;
use crate::room_data::Rooms;
use crate::util::ecies::decrypt_with_symmetric_key;
use ciborium::de::from_reader;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::ReadableExt;

use freenet_stdlib::client_api::{ContractResponse, HostResponse};
use freenet_stdlib::prelude::OutboundDelegateMsg;
pub use get_response::handle_get_response;
pub use put_response::handle_put_response;
use river_core::chat_delegate::{ChatDelegateRequestMsg, ChatDelegateResponseMsg, OutboundDmStore};
use river_core::room_state::member::MemberId;
use river_core::room_state::message::{MessageId, RoomMessageBody};
use river_core::room_state::privacy::PrivacyMode;
use std::collections::HashMap;
pub use subscribe_response::handle_subscribe_response;
pub use update_notification::handle_update_notification;
pub use update_response::handle_update_response;

/// Handles responses from the Freenet API
pub struct ResponseHandler {
    room_synchronizer: RoomSynchronizer,
}

/// Response flags returned from handle_api_response
#[derive(Default)]
pub struct ResponseFlags {
    /// True if a re-PUT should be scheduled (subscription failed but we have local state)
    pub needs_reput: bool,
    /// True if initial sync was kicked off for rooms loaded from delegate
    /// storage and a subscription-timeout check should be scheduled.
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
                    if subscribed {
                        // Fetch current contract state after successful subscribe.
                        // On reconnect, rooms loaded from delegate storage may be stale.
                        // Subscribe doesn't return state, so we need an explicit GET.
                        if let Err(e) = self.room_synchronizer.get_contract_state(&key).await {
                            error!(
                                "Failed to GET state after subscribe for {}: {}",
                                key.id(),
                                e
                            );
                        }
                    }
                }
                ContractResponse::NotFound { instance_id } => {
                    // The network has no peer hosting the contract at this key.
                    // Most likely cause: the room was created against an older
                    // room-contract WASM generation (see
                    // `common/legacy_room_contracts.toml`), so the current
                    // client derives a different key from the same owner_vk
                    // and finds no state at it.
                    //
                    // For a CURRENT room-contract key: resolve owner_vk via
                    // the same fallback chain handle_get_response uses
                    // (SYNC_INFO → PENDING_INVITES → ROOMS), then trigger
                    // the backward probe machinery — it walks
                    // legacy_contract_keys_for_owner newest-first, GETs each,
                    // merges the recovered state, and PUTs it under the
                    // current key (permissionless migration per AGENTS.md).
                    //
                    // For a LEGACY probe key (the probe's own GETs also come
                    // back here on NotFound): is_probe_instance is true and
                    // the probe's 12 s watchdog will advance it on schedule.
                    // We could short-circuit here to make 25-hop probes
                    // resolve in seconds instead of minutes, but constructing
                    // a ContractKey from just the instance_id needs the code
                    // hash (which the probe knows but doesn't expose). Skip
                    // the optimisation for now; the watchdog still drives
                    // the probe to completion.
                    use crate::components::app::freenet_api::backward_probe::{
                        is_probe_instance, start_backward_probe,
                    };
                    use crate::components::app::sync_info::SYNC_INFO;
                    use crate::util::owner_vk_to_contract_key;
                    use river_core::room_state::member::MemberId;

                    if is_probe_instance(&instance_id) {
                        info!(
                            "NotFound for legacy-probe contract {} — letting the watchdog advance the probe",
                            instance_id
                        );
                        return Ok(flags);
                    }

                    let owner_vk = SYNC_INFO
                        .read()
                        .get_owner_vk_for_instance_id(&instance_id)
                        .or_else(|| {
                            // Fallback 1: pending invites — the user just
                            // pasted an invitation and the room is in
                            // PENDING_INVITES but not yet in SYNC_INFO if
                            // the GET fired before SYNC_INFO registration
                            // completed.
                            let pending = crate::components::app::PENDING_INVITES.read();
                            pending.map.keys().find_map(|owner| {
                                if owner_vk_to_contract_key(owner).id() == &instance_id {
                                    Some(*owner)
                                } else {
                                    None
                                }
                            })
                        })
                        .or_else(|| {
                            // Fallback 2: ROOMS — covers in-app refreshes
                            // for rooms loaded from delegate storage.
                            ROOMS.read().map.iter().find_map(|(owner, rd)| {
                                if rd.contract_key.id() == &instance_id {
                                    Some(*owner)
                                } else {
                                    None
                                }
                            })
                        });

                    if let Some(owner_vk) = owner_vk {
                        info!(
                            "NotFound at current contract key for {:?} — starting backward probe \
                             over legacy generations (freenet/river#292)",
                            MemberId::from(owner_vk)
                        );
                        // An invitation-accept has no local snapshot to ride
                        // along — pass default(); the probe will CRDT-merge
                        // whatever legacy state it recovers with this empty
                        // baseline.
                        let _ = start_backward_probe(owner_vk, Default::default());
                    } else {
                        info!(
                            "NotFound for contract id {} with no resolvable owner_vk — \
                             no probe started",
                            instance_id
                        );
                    }
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
                                    // EnsureRoomSubscriptionResponse: routed through
                                    // the pending-request registry so callers awaiting
                                    // the delegate's ACK can clear their per-session
                                    // dedup on Err and retry (Bug #6). Previously this
                                    // was fire-and-forget, which combined with the
                                    // signing-key/EnsureRoomSubscription parallel-spawn
                                    // race below to leave the owner's delegate
                                    // permanently unsubscribed.
                                    //
                                    // We route on `(room_owner_vk, request_id)` so
                                    // concurrent or sequential calls for the same
                                    // room can't collide on the same registry slot —
                                    // PR #276 review feedback addressed the
                                    // `room_owner_vk`-only collision risk.
                                    ChatDelegateResponseMsg::EnsureRoomSubscriptionResponse {
                                        room_owner_vk,
                                        request_id,
                                        ..
                                    } => crate::components::app::chat_delegate::complete_pending_room_subscription_request(
                                        room_owner_vk,
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
                                                            // Gate legacy migration on whether
                                                            // current holds authoritative state
                                                            // (freenet/river#253). See
                                                            // `decide_legacy_migration_action`
                                                            // for the rules and rationale.
                                                            match decide_legacy_migration_action(
                                                                true,
                                                                !loaded_rooms.map.is_empty(),
                                                                !loaded_rooms
                                                                    .removed_rooms
                                                                    .is_empty(),
                                                            ) {
                                                                LegacyMigrationAction::MarkDone => {
                                                                    info!(
                                                                        "Current delegate has rooms_data — marking legacy migration done"
                                                                    );
                                                                    mark_legacy_migration_done();
                                                                    info!("Successfully loaded rooms from delegate");
                                                                }
                                                                LegacyMigrationAction::FireMigration => {
                                                                    info!(
                                                                        "Current delegate has empty rooms_data — firing legacy migration"
                                                                    );
                                                                    crate::util::safe_spawn_local(async {
                                                                        fire_legacy_migration_request().await;
                                                                    });
                                                                }
                                                            }
                                                        }

                                                        // Tombstone filter for all downstream loops.
                                                        // Includes both: (a) tombstones in the
                                                        // incoming loaded_rooms, and (b) tombstones
                                                        // already in the current in-memory ROOMS —
                                                        // because legacy delegates predate the
                                                        // tombstone field, the receiver's set is
                                                        // the authoritative one (freenet/river#247).
                                                        let tombstoned: std::collections::HashSet<
                                                            ed25519_dalek::VerifyingKey,
                                                        > = {
                                                            let mut t =
                                                                loaded_rooms.removed_rooms.clone();
                                                            let cur = ROOMS.read();
                                                            for vk in &cur.removed_rooms {
                                                                t.insert(*vk);
                                                            }
                                                            t
                                                        };

                                                        // Restore the current room selection if saved.
                                                        // Gated by `decide_current_room_restore`,
                                                        // which blocks:
                                                        //   (a) tombstoned saved rooms
                                                        //       (skeptical-review H2),
                                                        //   (b) overwriting a selection the user
                                                        //       has already made this session
                                                        //       (freenet/river#255),
                                                        //   (c) legacy-delegate responses
                                                        //       restoring cursor at all — the
                                                        //       legacy snapshot's cursor is years
                                                        //       stale and should never dictate
                                                        //       current selection (freenet/river#255).
                                                        let user_has_selected =
                                                            CURRENT_ROOM.read().owner_key.is_some();
                                                        let saved_key =
                                                            loaded_rooms.current_room_key;
                                                        let saved_tombstoned = saved_key
                                                            .map(|k| tombstoned.contains(&k))
                                                            .unwrap_or(false);
                                                        match decide_current_room_restore(
                                                            saved_key.is_some(),
                                                            saved_tombstoned,
                                                            user_has_selected,
                                                            is_legacy_delegate,
                                                        ) {
                                                            CurrentRoomRestore::Restore => {
                                                                let saved_room_key =
                                                                    saved_key.expect(
                                                                        "Restore implies saved_key present",
                                                                    );
                                                                info!("Restoring current room selection from delegate");
                                                                crate::util::defer(move || {
                                                                    *CURRENT_ROOM.write() =
                                                                        CurrentRoom {
                                                                            owner_key: Some(
                                                                                saved_room_key,
                                                                            ),
                                                                        };
                                                                });
                                                            }
                                                            CurrentRoomRestore::SkipNoSavedKey => {}
                                                            CurrentRoomRestore::SkipTombstoned => {
                                                                info!("Skipping current-room restore — saved room was left");
                                                            }
                                                            CurrentRoomRestore::SkipUserAlreadySelected => {
                                                                info!("Skipping current-room restore — user has already selected a room this session (freenet/river#255)");
                                                            }
                                                            CurrentRoomRestore::SkipLegacyDelegate => {
                                                                info!("Skipping current-room restore — legacy delegate response should not dictate cursor (freenet/river#255)");
                                                            }
                                                        }

                                                        // Collect room keys and signing keys before merge
                                                        // (must extract before loaded_rooms is moved into defer)
                                                        // Filter out tombstoned rooms so we don't
                                                        // re-subscribe / re-sync rooms the user
                                                        // explicitly left (skeptical-review H1).
                                                        let room_keys: Vec<_> = loaded_rooms
                                                            .map
                                                            .keys()
                                                            .copied()
                                                            .filter(|k| !tombstoned.contains(k))
                                                            .collect();
                                                        // Per-room signing-key migration inputs. For
                                                        // owner-mode rooms we also need to know the
                                                        // contract id so we can chain
                                                        // EnsureRoomSubscription onto the migration
                                                        // task — chained so the delegate is guaranteed
                                                        // to have the signing key on file before it
                                                        // sees the subscribe request (Bug #6). The
                                                        // previous design fired both requests as
                                                        // independent `safe_spawn_local` tasks and the
                                                        // race ordering was non-deterministic, so the
                                                        // delegate's "no signing key on file" reject
                                                        // path silently aborted the subscription on
                                                        // every cold load.
                                                        let signing_keys: Vec<_> = loaded_rooms
                                                            .map
                                                            .iter()
                                                            .filter(|(key, _)| {
                                                                !tombstoned.contains(*key)
                                                            })
                                                            .map(|(key, room_data)| {
                                                                let owns_room = room_data.owner_vk
                                                                    == room_data
                                                                        .self_sk
                                                                        .verifying_key();
                                                                // Derive the contract id from the
                                                                // CURRENT bundled room-contract WASM,
                                                                // NOT from `room_data.contract_key`.
                                                                // `room_data.contract_key` is the
                                                                // contract id captured at the time
                                                                // the room was last saved to the
                                                                // delegate's `rooms_data` blob — if
                                                                // the bundled WASM has changed since
                                                                // then, `Rooms::merge()` (called from
                                                                // the deferred closure below) will
                                                                // call `regenerate_contract_key()`
                                                                // and migrate the key to the new
                                                                // WASM's hash. Using the stale
                                                                // pre-merge key here would subscribe
                                                                // the delegate to the OLD contract,
                                                                // which no longer exists on the
                                                                // network — defeating the entire
                                                                // Bug #6 fix on any cold-load that
                                                                // happens to coincide with a
                                                                // room-contract WASM rebuild. Codex
                                                                // P1 finding on PR #276 round 2.
                                                                let contract_id_for_owner: Option<
                                                                    [u8; 32],
                                                                > = if owns_room {
                                                                    Some(
                                                                        **crate::util::owner_vk_to_contract_key(
                                                                            &room_data.owner_vk,
                                                                        )
                                                                        .id(),
                                                                    )
                                                                } else {
                                                                    None
                                                                };
                                                                (
                                                                    *key,
                                                                    room_data.room_key(),
                                                                    room_data.self_sk.clone(),
                                                                    contract_id_for_owner,
                                                                )
                                                            })
                                                            .collect();

                                                        // Merge the loaded rooms with the current rooms
                                                        crate::util::defer(move || {
                                                            ROOMS.with_mut(|current_rooms| {
                                                            if let Err(e) = current_rooms.merge(loaded_rooms) {
                                                                error!("Failed to merge rooms: {}", e);
                                                            } else {
                                                                info!("Successfully merged rooms from delegate");

                                                                // Re-decrypt ALL secret versions for each room (secrets are #[serde(skip)])
                                                                for room_data in current_rooms.map.values_mut() {
                                                                    let decrypted = room_data.repopulate_secrets_from_state();
                                                                    if room_data.is_private() {
                                                                        info!(
                                                                            "LoadRooms merge: decrypted {} room secret(s) for {:?}",
                                                                            decrypted,
                                                                            MemberId::from(&room_data.self_sk.verifying_key())
                                                                        );
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
                                                        });

                                                        // Mark current room as read since user is viewing it
                                                        // (must be after merge so room data exists)
                                                        crate::util::defer(|| {
                                                            mark_current_room_as_read();
                                                            update_document_title();
                                                        });

                                                        // Migrate signing keys to delegate for each loaded room
                                                        // (uses pre-extracted signing_keys since ROOMS merge is deferred)
                                                        info!("Migrating signing keys to delegate for {} rooms", signing_keys.len());
                                                        for (
                                                            room_key,
                                                            delegate_room_key,
                                                            signing_key,
                                                            owner_contract_id,
                                                        ) in &signing_keys
                                                        {
                                                            {
                                                                // Spawn async migration task via
                                                                // `safe_spawn_local`: per AGENTS.md
                                                                // "Dioxus WASM Signal Safety", direct
                                                                // `spawn_local` from inside a polled
                                                                // future causes RefCell re-entrancy
                                                                // panics on Firefox mobile.
                                                                let room_key_copy = *room_key;
                                                                let delegate_room_key =
                                                                    *delegate_room_key;
                                                                let signing_key =
                                                                    signing_key.clone();
                                                                let owner_contract_id =
                                                                    *owner_contract_id;
                                                                crate::util::safe_spawn_local(
                                                                    async move {
                                                                        let result = crate::signing::migrate_signing_key(
                                                                        delegate_room_key,
                                                                        &signing_key,
                                                                    )
                                                                    .await;

                                                                        if result != crate::signing::MigrationResult::Failed {
                                                                            // Must defer signal mutations from spawn_local to
                                                                            // avoid RefCell already borrowed panics in Dioxus runtime
                                                                            crate::util::defer(move || {
                                                                                let mut sanitized = false;
                                                                                ROOMS.with_mut(|rooms| {
                                                                                if let Some(room_data) = rooms.map.get_mut(&room_key_copy) {
                                                                                    room_data.key_migrated_to_delegate = true;
                                                                                    let params = river_core::room_state::ChatRoomParametersV1 {
                                                                                        owner: room_key_copy,
                                                                                    };
                                                                                    let removed = crate::signing::remove_unverifiable_messages(
                                                                                        &mut room_data.room_state,
                                                                                        &params,
                                                                                    );
                                                                                    sanitized = removed > 0;
                                                                                }
                                                                            });
                                                                                if sanitized {
                                                                                    crate::components::app::mark_needs_sync(room_key_copy);
                                                                                }
                                                                            });
                                                                        }

                                                                        // For owner-mode rooms, chain
                                                                        // EnsureRoomSubscription onto
                                                                        // the (just-completed) signing
                                                                        // key migration. Sequencing
                                                                        // ensures the delegate's
                                                                        // "no signing key on file"
                                                                        // reject path can't trip on a
                                                                        // race with `StoreSigningKey`
                                                                        // (Bug #6). The
                                                                        // `migrate_signing_key` call
                                                                        // above either confirmed an
                                                                        // existing key or stored a
                                                                        // fresh one, so the delegate's
                                                                        // signing-key probe will
                                                                        // succeed by the time
                                                                        // EnsureRoomSubscription lands.
                                                                        //
                                                                        // Skip on `MigrationResult::Failed`:
                                                                        // a Failed migration means
                                                                        // the delegate refused to
                                                                        // confirm/store the signing
                                                                        // key (transport down,
                                                                        // delegate not registered,
                                                                        // or signature mismatch), so
                                                                        // `EnsureRoomSubscription`
                                                                        // would either be rejected
                                                                        // with the same "no signing
                                                                        // key on file" error or
                                                                        // simply time out. Firing
                                                                        // anyway produced log spam
                                                                        // per cold-load × per
                                                                        // owned-room when the delegate
                                                                        // was persistently
                                                                        // unreachable (PR #276
                                                                        // review feedback). The
                                                                        // trade-off: we lose the
                                                                        // theoretical "stale key
                                                                        // still on file even though
                                                                        // verify failed" recovery
                                                                        // path, but in practice
                                                                        // that's vanishingly rare,
                                                                        // and the next cold-load
                                                                        // after the user reconnects
                                                                        // will retry from scratch
                                                                        // (the per-session dedup
                                                                        // resets across reloads).
                                                                        if let Some(contract_id) =
                                                                            owner_contract_id
                                                                        {
                                                                            if result == crate::signing::MigrationResult::Failed {
                                                                                warn!(
                                                                                    "Skipping EnsureRoomSubscription for {:?} — signing-key migration failed (delegate likely unreachable). Will retry on next cold load.",
                                                                                    delegate_room_key
                                                                                );
                                                                            } else {
                                                                                match crate::components::app::chat_delegate::ensure_room_subscription_once(
                                                                                    delegate_room_key,
                                                                                    contract_id,
                                                                                )
                                                                                .await
                                                                                {
                                                                                    Ok(true) => info!(
                                                                                        "Delegate subscribed to owner-mode room after signing-key migration"
                                                                                    ),
                                                                                    Ok(false) => info!(
                                                                                        "Skipped EnsureRoomSubscription for {:?} (already succeeded this session)",
                                                                                        delegate_room_key
                                                                                    ),
                                                                                    Err(e) => warn!(
                                                                                        "EnsureRoomSubscription failed for {:?}: {} (will retry on next load)",
                                                                                        delegate_room_key,
                                                                                        e
                                                                                    ),
                                                                                }
                                                                            }
                                                                        }
                                                                    },
                                                                );
                                                            }
                                                        }

                                                        // Mark all loaded rooms as having completed initial sync
                                                        // and subscribe to receive updates
                                                        for room_key in &room_keys {
                                                            let room_key_copy = *room_key;
                                                            crate::util::defer(move || {
                                                                mark_initial_sync_complete(
                                                                    &room_key_copy,
                                                                );
                                                            });
                                                        }

                                                        // EnsureRoomSubscription for owner-mode rooms
                                                        // is now chained off the signing-key
                                                        // migration spawn above (Bug #6 race fix).
                                                        // The delegate handles asynchronous
                                                        // "background catch-up" rotations (auto-prune
                                                        // from message lifecycle, peer state updates
                                                        // while the UI is closed); the UI continues
                                                        // to drive ban and manual rotations
                                                        // synchronously. `ensure_room_subscription_once`
                                                        // dedups per-session so re-loads of
                                                        // `rooms_data` don't spam the delegate, but
                                                        // clears the dedup entry on Err so a future
                                                        // load can retry.

                                                        // Drive initial sync for rooms loaded
                                                        // from delegate storage through
                                                        // process_rooms() — do NOT subscribe to the
                                                        // contract directly here.
                                                        //
                                                        // A bare Subscribe is REJECTED by the node
                                                        // when the contract's WASM/parameters are
                                                        // not cached locally (freenet-core#3601) —
                                                        // always the case for a room restored from
                                                        // an exported backup, or any room not used
                                                        // on this node before. The rejection comes
                                                        // back as an Err API response, NOT a
                                                        // SubscribeResponse{subscribed:false}, so
                                                        // the re-PUT recovery in
                                                        // handle_subscribe_response never runs and
                                                        // the room is stuck on "Syncing room state
                                                        // from the network..." forever
                                                        // (freenet/river#287).
                                                        //
                                                        // process_rooms() ->
                                                        // rooms_awaiting_subscription() does the
                                                        // right thing per room: imported rooms
                                                        // (default placeholder state) GET with
                                                        // return_contract_code=true; full-state
                                                        // rooms PUT with subscribe=true. Both cache
                                                        // the contract WASM on the node BEFORE any
                                                        // subscribe, so the subscription succeeds.
                                                        let had_loaded_rooms =
                                                            !room_keys.is_empty();
                                                        info!(
                                                            "Scheduling initial sync for {} rooms loaded from delegate",
                                                            room_keys.len()
                                                        );
                                                        for room_key in room_keys {
                                                            // Queues a ProcessRooms cycle via the
                                                            // NEEDS_SYNC effect. mark_needs_sync
                                                            // defers the signal write
                                                            // (setTimeout(0)), so it runs AFTER
                                                            // the deferred ROOMS merge above — the
                                                            // effect therefore sees the merged
                                                            // rooms and process_rooms() picks each
                                                            // one up as Disconnected.
                                                            crate::components::app::mark_needs_sync(
                                                                room_key,
                                                            );
                                                        }
                                                        if had_loaded_rooms {
                                                            // Schedule a subscription-timeout
                                                            // check so a room whose PUT/GET
                                                            // response never arrives is reset and
                                                            // retried by rooms_awaiting_subscription().
                                                            flags.subscriptions_initiated = true;
                                                        }

                                                        // TODO: Remove legacy migration code after 2026-03-01
                                                        // If this was from the legacy delegate, save to the new delegate
                                                        if is_legacy_delegate {
                                                            info!("Migrating room data from legacy delegate to new delegate");
                                                            crate::util::safe_spawn_local(async {
                                                                match save_rooms_to_delegate().await
                                                                {
                                                                    Ok(_) => {
                                                                        info!("Successfully migrated room data to new delegate");
                                                                        mark_legacy_migration_done(
                                                                        );
                                                                    }
                                                                    Err(e) => {
                                                                        error!("Failed to migrate room data to new delegate: {}", e);
                                                                        // Don't mark as done - will retry on next startup
                                                                    }
                                                                }
                                                            });
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
                                                } else {
                                                    // Current delegate has no `rooms_data` record
                                                    // at all. Per `decide_legacy_migration_action`,
                                                    // this is the FireMigration case — safe
                                                    // because there is no current state to clobber
                                                    // (freenet/river#253).
                                                    debug_assert_eq!(
                                                        decide_legacy_migration_action(
                                                            false, false, false
                                                        ),
                                                        LegacyMigrationAction::FireMigration,
                                                    );
                                                    info!(
                                                        "Current delegate empty — firing legacy migration"
                                                    );
                                                    crate::util::safe_spawn_local(async {
                                                        fire_legacy_migration_request().await;
                                                    });
                                                }
                                            }
                                        } else if key.as_bytes() == OUTBOUND_DMS_STORAGE_KEY {
                                            // Outbound DM plaintext cache (#256).
                                            // Both the current delegate and any
                                            // legacy delegate use the same key —
                                            // legacy responses get merged into the
                                            // current cache and then re-saved so
                                            // the migration is one-way.
                                            handle_outbound_dms_get_response(
                                                value,
                                                is_legacy_delegate,
                                            );
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
                                    ChatDelegateResponseMsg::EnsureRoomSubscriptionResponse {
                                        room_owner_vk,
                                        result,
                                        ..
                                    } => match result {
                                        Ok(_) => info!(
                                            "Delegate confirmed subscription for room: {:?}",
                                            room_owner_vk
                                        ),
                                        Err(e) => warn!(
                                            "Delegate failed to subscribe to room {:?}: {}",
                                            room_owner_vk, e
                                        ),
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

/// Process a `GetResponse` for `OUTBOUND_DMS_STORAGE_KEY` (issue
/// freenet/river#256). Hydrates the in-memory cache from the
/// serialized `OutboundDmStore`, and — when the response came from a
/// legacy delegate — schedules a save so the migrated entries land
/// under the current delegate's key.
fn handle_outbound_dms_get_response(value: Option<Vec<u8>>, is_legacy_delegate: bool) {
    let Some(bytes) = value else {
        info!(
            "No outbound-DMs blob present in delegate ({})",
            if is_legacy_delegate {
                "legacy"
            } else {
                "current"
            }
        );
        return;
    };

    match from_reader::<OutboundDmStore, _>(&bytes[..]) {
        Ok(store) => {
            let hidden_count = hydrate_hidden_dm_threads(store.hidden_threads);
            let count = hydrate_outbound_dms_cache(store.entries);
            info!(
                "Hydrated {} outbound-DM entries and {} hidden DM thread entries from {} delegate",
                count,
                hidden_count,
                if is_legacy_delegate {
                    "legacy"
                } else {
                    "current"
                }
            );
            if is_legacy_delegate && (count > 0 || hidden_count > 0) {
                // Persist the merged cache under the current delegate
                // key so subsequent loads find the data without
                // re-hitting the legacy delegate.
                crate::util::safe_spawn_local(async {
                    if let Err(e) = save_outbound_dms_to_delegate().await {
                        warn!("Failed to migrate outbound-DMs to current delegate: {}", e);
                    }
                });
            }

            // Codex P2 fix on PR #259 re-review: when this response
            // (outbound_dms) arrives AFTER rooms_data, the
            // ROOMS-subscribed `App()` prune effect already fired on
            // an empty cache, and intentionally does NOT subscribe to
            // OUTBOUND_DMS (would loop on its own writes). Without a
            // follow-up prune here, hydrated entries whose tokens are
            // already listed in a recipient's purge envelope persist
            // in the cache until some unrelated ROOMS change happens
            // to re-trigger the effect.
            //
            // Defer so this runs AFTER `hydrate_outbound_dms_cache`'s
            // own internal `defer(with_mut)` insert has fired —
            // setTimeout(0) macrotasks execute in FIFO enqueue order,
            // so the prune sees the freshly-hydrated entries.
            if count > 0 {
                crate::util::defer(|| {
                    prune_outbound_dms_for_purges();
                });
            }
        }
        Err(e) => {
            error!("Failed to deserialize outbound-DMs blob: {}", e);
        }
    }
}

/// The action to take after observing a `current_room_key` carried in a
/// `rooms_data` `GetResponse` from the chat delegate. Returned by
/// [`decide_current_room_restore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CurrentRoomRestore {
    /// Saved cursor is present, not tombstoned, the user has not already
    /// selected a room this session, and the response is from the
    /// current (non-legacy) delegate. Safe to write `CURRENT_ROOM`.
    Restore,
    /// No `current_room_key` was carried — nothing to restore.
    SkipNoSavedKey,
    /// Saved cursor points at a room the receiver has tombstoned
    /// (the user explicitly left it). Restoring would silently re-enter
    /// a left room. See freenet/river#247 / skeptical-review H2.
    SkipTombstoned,
    /// The user has already selected a room in `CURRENT_ROOM` this
    /// session (e.g. clicked Room A on the rooms list). A late-arriving
    /// delegate response carrying a different `current_room_key` must
    /// NOT overwrite that selection. See freenet/river#255.
    SkipUserAlreadySelected,
    /// This response is from a *legacy* delegate. Its cursor is a
    /// snapshot from a previous build's run and is years-stale by
    /// construction — it should never dictate current selection,
    /// regardless of whether the user has clicked yet. See
    /// freenet/river#255.
    SkipLegacyDelegate,
}

/// Decide whether to restore `CURRENT_ROOM` from a `rooms_data` snapshot's
/// `current_room_key`. Pure function — caller threads in the relevant
/// signals so this is unit-testable without a Dioxus runtime.
///
/// Priority order (highest precedence first), mirroring the variants of
/// [`CurrentRoomRestore`]:
///
/// 1. `!saved_key_present` → `SkipNoSavedKey` (nothing to restore).
/// 2. `is_legacy_delegate` → `SkipLegacyDelegate` (legacy cursor is
///    structurally stale; never let it touch current selection).
/// 3. `user_has_selected` → `SkipUserAlreadySelected` (the user's
///    explicit click wins over a delegate-restored cursor —
///    freenet/river#255).
/// 4. `saved_tombstoned` → `SkipTombstoned` (saved room was left —
///    freenet/river#247).
/// 5. Otherwise → `Restore`.
///
/// The legacy-delegate gate is intentionally above the
/// already-selected gate so legacy cursor restoration is blocked even on
/// a fresh session where the user has not clicked yet — that matches the
/// issue author's "Optionally also skip when is_legacy_delegate == true"
/// note and prevents a legacy GetResponse arriving before the user
/// interacts from silently switching the rooms list to a years-stale
/// selection.
pub(crate) fn decide_current_room_restore(
    saved_key_present: bool,
    saved_tombstoned: bool,
    user_has_selected: bool,
    is_legacy_delegate: bool,
) -> CurrentRoomRestore {
    if !saved_key_present {
        return CurrentRoomRestore::SkipNoSavedKey;
    }
    if is_legacy_delegate {
        return CurrentRoomRestore::SkipLegacyDelegate;
    }
    if user_has_selected {
        return CurrentRoomRestore::SkipUserAlreadySelected;
    }
    if saved_tombstoned {
        return CurrentRoomRestore::SkipTombstoned;
    }
    CurrentRoomRestore::Restore
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Baseline: current delegate, no prior selection, saved key not
    /// tombstoned. Cursor restore should fire.
    #[test]
    fn restore_when_current_delegate_and_no_prior_selection() {
        assert_eq!(
            decide_current_room_restore(
                /* saved_key_present */ true, /* saved_tombstoned */ false,
                /* user_has_selected */ false, /* is_legacy_delegate */ false,
            ),
            CurrentRoomRestore::Restore,
        );
    }

    /// No cursor in the snapshot → nothing to restore. Takes precedence
    /// over every other gate so the caller does not need a separate
    /// `saved_key.is_some()` check.
    #[test]
    fn skip_when_no_saved_key() {
        // is_legacy_delegate=true AND user_has_selected=true AND tombstoned
        // — none of those matter without a saved key.
        assert_eq!(
            decide_current_room_restore(false, true, true, true),
            CurrentRoomRestore::SkipNoSavedKey,
        );
    }

    /// freenet/river#255 regression test, primary case:
    /// session sequence (1) fresh load, (2) user clicks Room A, (3)
    /// legacy GET response arrives carrying a stale Room B in
    /// `current_room_key`. The deferred write previously overwrote the
    /// user's selection with Room B. Now we skip the restore.
    #[test]
    fn issue_255_user_selection_not_overwritten_by_current_delegate_cursor() {
        assert_eq!(
            decide_current_room_restore(
                /* saved_key_present */ true, /* saved_tombstoned */ false,
                /* user_has_selected */ true, /* is_legacy_delegate */ false,
            ),
            CurrentRoomRestore::SkipUserAlreadySelected,
        );
    }

    /// freenet/river#255: a *legacy* delegate's cursor must NEVER be
    /// restored — even on a fresh session before the user has clicked.
    /// The legacy snapshot is years-stale by construction.
    #[test]
    fn issue_255_legacy_delegate_never_restores_cursor_even_without_prior_selection() {
        assert_eq!(
            decide_current_room_restore(
                /* saved_key_present */ true, /* saved_tombstoned */ false,
                /* user_has_selected */ false, /* is_legacy_delegate */ true,
            ),
            CurrentRoomRestore::SkipLegacyDelegate,
        );
    }

    /// freenet/river#255: a legacy cursor that *also* races a user
    /// click is doubly bad. Either gate is sufficient; the legacy gate
    /// takes precedence in the result variant.
    #[test]
    fn issue_255_legacy_gate_precedes_user_selection_gate() {
        assert_eq!(
            decide_current_room_restore(
                /* saved_key_present */ true, /* saved_tombstoned */ false,
                /* user_has_selected */ true, /* is_legacy_delegate */ true,
            ),
            CurrentRoomRestore::SkipLegacyDelegate,
        );
    }

    /// freenet/river#247 / skeptical-review H2 regression: saved
    /// cursor points at a room the receiver has tombstoned. Skip
    /// restore so we do not silently re-enter a left room.
    #[test]
    fn skip_when_saved_room_was_tombstoned() {
        assert_eq!(
            decide_current_room_restore(
                /* saved_key_present */ true, /* saved_tombstoned */ true,
                /* user_has_selected */ false, /* is_legacy_delegate */ false,
            ),
            CurrentRoomRestore::SkipTombstoned,
        );
    }

    /// User-selection gate fires even when the saved room is also
    /// tombstoned. The user-selection result is preferred so the log
    /// message accurately describes the highest-precedence reason.
    #[test]
    fn user_selection_gate_precedes_tombstone_gate() {
        assert_eq!(
            decide_current_room_restore(
                /* saved_key_present */ true, /* saved_tombstoned */ true,
                /* user_has_selected */ true, /* is_legacy_delegate */ false,
            ),
            CurrentRoomRestore::SkipUserAlreadySelected,
        );
    }

    /// Regression guard for freenet/river#287.
    ///
    /// A user restoring old room identities from exported backups hit a
    /// never-ending "Syncing room state from the network..." spinner.
    /// Root cause: the rooms-loaded-from-delegate path issued a bare
    /// `Subscribe` request for every room. The node REJECTS a Subscribe
    /// when the contract's WASM/parameters are not cached locally
    /// (freenet-core#3601) — always true for a freshly-restored room.
    /// That rejection arrives as an `Err` API response, not a
    /// `SubscribeResponse { subscribed: false }`, so the re-PUT recovery
    /// in `handle_subscribe_response` never runs and the room hangs.
    ///
    /// The fix routes rooms loaded from delegate storage through
    /// `process_rooms()` (via `mark_needs_sync`), which PUTs full-state
    /// rooms or GETs imported rooms with `return_contract_code = true` —
    /// both cache the contract WASM on the node BEFORE any subscribe.
    ///
    /// `handle_api_response` is too deeply coupled to the Dioxus signal
    /// runtime + WebSocket to drive in a unit test, so this is a
    /// source-text pin on this file's PRODUCTION code (same approach as
    /// the #267 guards in `room_synchronizer.rs`). It fails if a future
    /// change reintroduces a direct subscribe, or drops the
    /// `process_rooms()` hand-off, in the rooms-loaded path.
    #[test]
    fn issue_287_loaded_rooms_do_not_bare_subscribe() {
        // include_str!() reads the WHOLE file, this test module included.
        // Slice it off at `mod tests {` so the literals in the test
        // itself can neither satisfy the positive assertion nor trip the
        // negative one — the pin must reflect production code only.
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("response_handler.rs must have production code before `mod tests`");

        assert!(
            !production.contains("subscribe_to_contract"),
            "the rooms-loaded-from-delegate path must not call \
             subscribe_to_contract: a bare Subscribe is rejected by the \
             node when the contract WASM is not cached locally, which \
             silently hangs sync for restored rooms (freenet/river#287). \
             Subscribe only AFTER a PUT/GET caches the contract — route \
             loaded rooms through process_rooms() instead."
        );

        // The rooms-loaded-from-delegate path must hand off to the
        // NEEDS_SYNC -> ProcessRooms cycle so process_rooms() drives the
        // contract-caching PUT/GET. If that block is removed or moved,
        // the marker disappears from production code and `expect` fires.
        let marker = "Scheduling initial sync for";
        let split_at = production.find(marker).expect(
            "the rooms-loaded-from-delegate sync hand-off must exist in \
             response_handler.rs production code — the #287 fix has been \
             removed or moved",
        );
        assert!(
            production[split_at..].contains("mark_needs_sync"),
            "the rooms-loaded-from-delegate path must call mark_needs_sync \
             to drive sync through process_rooms() (freenet/river#287)."
        );
    }
}
