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
    cas_store_correlation_key, complete_pending_public_key_request, complete_pending_request,
    complete_pending_sign_request, complete_pending_signing_key_request,
    decide_legacy_migration_action, fire_legacy_migration_request, get_versioned_correlation_key,
    hydrate_hidden_dm_threads, hydrate_outbound_dms_cache, is_legacy_delegate_key,
    mark_legacy_migration_done, parse_room_storage_key, prune_outbound_dms_for_purges,
    room_storage_key, save_outbound_dms_to_delegate, save_rooms_to_delegate, send_delegate_request,
    send_delegate_request_to, LegacyMigrationAction, OUTBOUND_DMS_STORAGE_KEY, ROOMS_META_KEY,
    ROOMS_STORAGE_KEY,
};
use crate::components::app::document_title::{mark_current_room_as_read, update_document_title};
use crate::components::app::notifications::mark_initial_sync_complete;
use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::room_data::CurrentRoom;
use crate::room_data::{RoomSlot, Rooms, RoomsMeta};
use crate::util::ecies::decrypt_with_symmetric_key;
use ciborium::de::from_reader;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::ReadableExt;

use freenet_stdlib::client_api::{ContractResponse, HostResponse};
use freenet_stdlib::prelude::{DelegateKey, OutboundDelegateMsg};
pub use get_response::handle_get_response;
pub use put_response::handle_put_response;
use river_core::chat_delegate::{
    CasStoreResult, ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg,
    OutboundDmStore,
};
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
                                    // CAS storage responses (freenet/river#345) correlate on
                                    // DISTINCT keys (prefix + storage key) so they can't be
                                    // confused with a concurrent plain Get/Store for the same
                                    // storage key. Rebuild the same correlation key the request
                                    // registered under.
                                    ChatDelegateResponseMsg::GetVersionedResponse { key, .. } => {
                                        let corr = river_core::chat_delegate::ChatDelegateKey::new(
                                            get_versioned_correlation_key(key),
                                        );
                                        complete_pending_request(&corr, response.clone())
                                    }
                                    ChatDelegateResponseMsg::CasStoreResponse { key, .. } => {
                                        let corr = river_core::chat_delegate::ChatDelegateKey::new(
                                            cas_store_correlation_key(key),
                                        );
                                        complete_pending_request(&corr, response.clone())
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
                                                        // NOTE: legacy-delegate migration is permanent infrastructure
                                                        // (every delegate-WASM bump needs it — freenet/river#345).
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

                                                        if hydrate_loaded_rooms(
                                                            loaded_rooms,
                                                            is_legacy_delegate,
                                                        ) {
                                                            flags.subscriptions_initiated = true;
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
                                                // NOTE: legacy-delegate migration is permanent — every delegate-WASM bump
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
                                        } else if key.as_bytes() == ROOMS_META_KEY
                                            || parse_room_storage_key(key.as_bytes()).is_some()
                                        {
                                            // Per-room load responses (room:<vk> /
                                            // rooms_meta) are consumed by the awaiting
                                            // orchestration task (load_rooms_per_room)
                                            // via the pending-request registry. The
                                            // processing match still runs for every
                                            // response, so swallow these here rather
                                            // than warning "unexpected key".
                                        } else {
                                            warn!(
                                                "Unexpected key in GetResponse: {:?}",
                                                String::from_utf8_lossy(key.as_bytes())
                                            );
                                        }
                                    }
                                    ChatDelegateResponseMsg::ListResponse { keys } => {
                                        info!("Listed {} delegate keys", keys.len());
                                        // Per-room room load (freenet/river#345 / #65).
                                        // The CURRENT delegate's List drives which
                                        // room:<vk> slots (+ rooms_meta) to fetch and
                                        // hydrate. A LEGACY delegate's List (fired by
                                        // fire_legacy_migration_request to discover the
                                        // dynamic per-room keys it holds) drives a
                                        // migration that re-saves those rooms to the
                                        // current delegate. Both spawn so the per-key
                                        // GETs can be awaited without blocking the message
                                        // loop (the responses arrive through this loop).
                                        if is_legacy_delegate {
                                            let legacy_key = key.clone();
                                            crate::util::safe_spawn_local(async move {
                                                migrate_legacy_per_room(legacy_key, keys).await;
                                            });
                                        } else {
                                            crate::util::safe_spawn_local(async move {
                                                load_rooms_per_room(keys).await;
                                            });
                                        }
                                    }
                                    // CAS storage responses (freenet/river#345). The
                                    // awaiting save loop in `do_save_rooms_to_delegate`
                                    // consumes these via the pending-request registry;
                                    // here we only log for visibility.
                                    ChatDelegateResponseMsg::GetVersionedResponse {
                                        key,
                                        value,
                                        generation,
                                    } => {
                                        info!(
                                            "GetVersioned key {:?}: present={}, generation={}",
                                            String::from_utf8_lossy(key.as_bytes()),
                                            value.is_some(),
                                            generation
                                        );
                                    }
                                    ChatDelegateResponseMsg::CasStoreResponse { key, result } => {
                                        match &result {
                                            CasStoreResult::Stored { generation } => info!(
                                                "CAS stored key {:?} at generation {}",
                                                String::from_utf8_lossy(key.as_bytes()),
                                                generation
                                            ),
                                            CasStoreResult::Conflict {
                                                current_generation, ..
                                            } => info!(
                                                "CAS conflict for key {:?}; current generation {}",
                                                String::from_utf8_lossy(key.as_bytes()),
                                                current_generation
                                            ),
                                            CasStoreResult::Failed(e) => warn!(
                                                "CAS store failed for key {:?}: {}",
                                                String::from_utf8_lossy(key.as_bytes()),
                                                e
                                            ),
                                        }
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

/// Per-room room load (freenet/river#345 / #65). Reconstructs the in-memory
/// `Rooms` from the current delegate's `room:<vk>` slot keys (+ `rooms_meta`)
/// discovered by a `ListRequest`, then hands the assembled value to the shared
/// [`hydrate_loaded_rooms`].
///
/// Runs in a `safe_spawn_local` task spawned from the `ListResponse` arm so the
/// per-key `GetRequest`s can be `await`ed without blocking the message loop
/// (their responses arrive through that same loop). Plain `GetRequest` is used
/// (not `GetVersionedRequest`) so the correlation key is the bare storage key —
/// distinct from a concurrent CAS save's `__get_versioned__:`/`__cas_store__:`
/// correlation, so an in-flight save for the same room can't steal this load's
/// response slot. The delegate strips the versioning envelope on plain Get, so
/// the bytes are the raw `RoomSlot` / `RoomsMeta` CBOR.
///
/// Three cases by what the List contains:
/// * **per-room keys present** → current delegate is authoritative: mark legacy
///   migration done, fetch every slot + meta, hydrate.
/// * **no per-room keys, but a single `rooms_data` blob** → a pre-per-room
///   snapshot under the current delegate (defensive; normally the new delegate
///   key is empty on first upgrade): read it, hydrate, then re-save so it
///   explodes into per-room keys. NON-DESTRUCTIVE — the blob is left in place as
///   a rollback fallback (`save_rooms_to_delegate` no longer writes it).
/// * **nothing** → empty current delegate: probe the legacy delegates (the old
///   WASM-key migration path), exactly as the old empty-`rooms_data` GetResponse
///   did.
async fn load_rooms_per_room(keys: Vec<ChatDelegateKey>) {
    match plan_load_from_keys(&keys) {
        LoadPlan::ProbeLegacy => {
            info!("Current delegate has no room data — firing legacy migration probe");
            fire_legacy_migration_request().await;
        }
        LoadPlan::MigrateCurrentBlob => {
            info!(
                "No per-room keys but a current-delegate rooms_data blob exists — \
                 migrating it to per-room keys"
            );
            migrate_current_blob_to_per_room().await;
        }
        LoadPlan::PerRoom { room_vks, has_meta } => {
            // Current delegate holds authoritative per-room data: never probe
            // legacy (freenet/river#253 — a legacy response could clobber newer
            // state).
            mark_legacy_migration_done();

            // Concurrency note: a WebSocket reconnect re-fires `ListRequest`, so
            // two `load_rooms_per_room` tasks can briefly overlap. The per-room
            // GETs register under bare-key correlation in the single-waiter
            // `PENDING_REQUESTS` map, so the second load's GET for a shared key
            // orphans the first's oneshot (that room logs an error and is skipped
            // in THAT pass). No permanent loss: each response is delivered to one
            // load and `Rooms::merge` is a union, so across the overlapping
            // passes every room still lands in ROOMS. (Load-vs-SAVE never
            // collides — those use distinct prefixed correlation keys.)
            info!("Loading {} per-room slot(s) from delegate", room_vks.len());
            let mut slots: Vec<(ed25519_dalek::VerifyingKey, RoomSlot)> = Vec::new();
            for vk in room_vks {
                match send_delegate_request(ChatDelegateRequestMsg::GetRequest {
                    key: ChatDelegateKey::new(room_storage_key(&vk)),
                })
                .await
                {
                    Ok(ChatDelegateResponseMsg::GetResponse {
                        value: Some(bytes), ..
                    }) => match from_reader::<RoomSlot, _>(&bytes[..]) {
                        Ok(slot) => slots.push((vk, slot)),
                        Err(e) => error!("Unparseable room slot for {:?}: {}", vk, e),
                    },
                    Ok(ChatDelegateResponseMsg::GetResponse { value: None, .. }) => {
                        // Listed in the index but the value is gone — skip it.
                        warn!("Room key {:?} listed but value missing", vk);
                    }
                    Ok(other) => error!("Unexpected response loading room {:?}: {:?}", vk, other),
                    Err(e) => error!("Failed to load room {:?}: {}", vk, e),
                }
            }

            let meta = if has_meta {
                match send_delegate_request(ChatDelegateRequestMsg::GetRequest {
                    key: ChatDelegateKey::new(ROOMS_META_KEY.to_vec()),
                })
                .await
                {
                    Ok(ChatDelegateResponseMsg::GetResponse {
                        value: Some(bytes), ..
                    }) => match from_reader::<RoomsMeta, _>(&bytes[..]) {
                        Ok(meta) => Some(meta),
                        Err(e) => {
                            error!("Unparseable rooms_meta: {}", e);
                            None
                        }
                    },
                    Ok(_) => {
                        warn!("rooms_meta listed but value missing");
                        None
                    }
                    Err(e) => {
                        error!("Failed to load rooms_meta: {}", e);
                        None
                    }
                }
            } else {
                None
            };

            let loaded = reconstruct_rooms(slots, meta);
            if hydrate_loaded_rooms(loaded, false) {
                schedule_subscription_timeout_check();
            }
        }
    }
}

/// What a `ListResponse` from the current delegate tells the load path to do.
/// Pure decision so it can be unit-tested without the websocket.
#[derive(Debug, PartialEq)]
enum LoadPlan {
    /// Per-room keys exist — fetch each slot (+ `rooms_meta` if `has_meta`).
    PerRoom {
        room_vks: Vec<ed25519_dalek::VerifyingKey>,
        has_meta: bool,
    },
    /// No per-room keys, but a single legacy `rooms_data` blob under the current
    /// delegate — read it and explode it into per-room keys.
    MigrateCurrentBlob,
    /// Nothing relevant stored — probe the legacy (old-WASM-key) delegates.
    ProbeLegacy,
}

/// Classify the current delegate's stored keys into a [`LoadPlan`]. Per-room
/// keys take priority over a stray legacy blob (a blob can only survive as a
/// rollback fallback once the per-room keys exist, so it must never re-trigger
/// the blob-migration path).
fn plan_load_from_keys(keys: &[ChatDelegateKey]) -> LoadPlan {
    let mut room_vks: Vec<ed25519_dalek::VerifyingKey> = Vec::new();
    let mut has_meta = false;
    let mut has_legacy_blob = false;
    for k in keys {
        let b = k.as_bytes();
        if let Some(vk) = parse_room_storage_key(b) {
            room_vks.push(vk);
        } else if b == ROOMS_META_KEY {
            has_meta = true;
        } else if b == ROOMS_STORAGE_KEY {
            has_legacy_blob = true;
        }
    }
    if !room_vks.is_empty() {
        LoadPlan::PerRoom { room_vks, has_meta }
    } else if has_legacy_blob {
        LoadPlan::MigrateCurrentBlob
    } else {
        LoadPlan::ProbeLegacy
    }
}

/// Reconstruct an in-memory [`Rooms`] from decoded per-room slots plus the
/// optional [`RoomsMeta`]. Pure (no I/O) so the per-room load round-trip is
/// unit-testable: `Present` → `map`, `Tombstone` → `removed_rooms`, then
/// `apply_meta` layers the view preferences (and prunes `room_order` to present
/// rooms).
fn reconstruct_rooms(
    slots: Vec<(ed25519_dalek::VerifyingKey, RoomSlot)>,
    meta: Option<RoomsMeta>,
) -> Rooms {
    let mut rooms = empty_rooms();
    for (vk, slot) in slots {
        match slot {
            RoomSlot::Present(room) => {
                rooms.map.insert(vk, *room);
            }
            RoomSlot::Tombstone => {
                rooms.removed_rooms.insert(vk);
            }
        }
    }
    if let Some(meta) = meta {
        rooms.apply_meta(meta);
    }
    rooms
}

/// Defensive single-blob → per-room migration for a `rooms_data` blob found
/// under the CURRENT delegate (see [`load_rooms_per_room`]). Reads the blob via
/// `GetVersionedRequest` — a distinct correlation key from the single-blob
/// `GetResponse` arm and from any save's CAS, and no save ever writes
/// `rooms_data`, so nothing else processes or races this read. After hydrating,
/// a `save_rooms_to_delegate()` writes the per-room keys; the blob is
/// intentionally left in place as a rollback fallback.
async fn migrate_current_blob_to_per_room() {
    match send_delegate_request(ChatDelegateRequestMsg::GetVersionedRequest {
        key: ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
    })
    .await
    {
        Ok(ChatDelegateResponseMsg::GetVersionedResponse {
            value: Some(bytes), ..
        }) => match from_reader::<Rooms, _>(&bytes[..]) {
            Ok(loaded) => {
                mark_legacy_migration_done();
                if hydrate_loaded_rooms(loaded, false) {
                    schedule_subscription_timeout_check();
                }
                // Explode into per-room keys (non-destructive: blob stays).
                // hydrate defers the ROOMS merge via `defer()` (setTimeout 0),
                // so the save MUST also be deferred — a direct `.await` here
                // reads the pre-merge (empty) ROOMS and would write nothing.
                // safe_spawn_local schedules a setTimeout(0) that, being queued
                // AFTER the merge's defer, runs once the merge has populated
                // ROOMS (FIFO ordering), mirroring the legacy-delegate re-save.
                crate::util::safe_spawn_local(async {
                    if let Err(e) = save_rooms_to_delegate().await {
                        error!("Failed to explode single blob into per-room keys: {}", e);
                    }
                });
            }
            Err(e) => error!("Failed to deserialize current-delegate rooms blob: {}", e),
        },
        Ok(_) => {
            // Index listed rooms_data but the value is gone — treat as empty.
            fire_legacy_migration_request().await;
        }
        Err(e) => warn!("Failed to read current-delegate rooms blob: {}", e),
    }
}

/// Migrate the per-room slots a LEGACY delegate holds onto the current delegate
/// (freenet/river#345 / #65). Driven by a legacy delegate's `ListResponse`
/// (fired fire-and-forget by `fire_legacy_migration_request`): the per-room keys
/// are dynamic, so the fixed `rooms_data`/`outbound_dms` probes can't discover
/// them. Without this, the first delegate-WASM bump AFTER per-room storage ships
/// would strand every room under the (now-legacy) per-room delegate key.
///
/// Reads each `room:<vk>` slot (+ `rooms_meta`) FROM the legacy delegate, then
/// hydrates with `is_legacy_delegate = true` so the cursor isn't restored from
/// the stale snapshot and the rooms are re-saved to the current delegate
/// (hydrate's legacy branch). The legacy delegate's data is left untouched. The
/// single-blob legacy format + `outbound_dms` are still handled by the fixed
/// probes, so this only acts when the legacy delegate actually has per-room keys.
async fn migrate_legacy_per_room(legacy_key: DelegateKey, keys: Vec<ChatDelegateKey>) {
    let (room_vks, has_meta) = match plan_load_from_keys(&keys) {
        LoadPlan::PerRoom { room_vks, has_meta } => (room_vks, has_meta),
        // No per-room keys on this legacy delegate — its single blob (if any) is
        // handled by the fixed rooms_data probe. Nothing per-room to migrate.
        LoadPlan::MigrateCurrentBlob | LoadPlan::ProbeLegacy => return,
    };

    info!(
        "Migrating {} per-room slot(s) from legacy delegate",
        room_vks.len()
    );
    let mut slots: Vec<(ed25519_dalek::VerifyingKey, RoomSlot)> = Vec::new();
    for vk in room_vks {
        match send_delegate_request_to(
            legacy_key.clone(),
            ChatDelegateRequestMsg::GetRequest {
                key: ChatDelegateKey::new(room_storage_key(&vk)),
            },
        )
        .await
        {
            Ok(ChatDelegateResponseMsg::GetResponse {
                value: Some(bytes), ..
            }) => match from_reader::<RoomSlot, _>(&bytes[..]) {
                Ok(slot) => slots.push((vk, slot)),
                Err(e) => error!("Unparseable legacy room slot for {:?}: {}", vk, e),
            },
            Ok(_) => warn!("Legacy room key {:?} listed but value missing", vk),
            Err(e) => error!("Failed to load legacy room {:?}: {}", vk, e),
        }
    }

    let meta = if has_meta {
        match send_delegate_request_to(
            legacy_key.clone(),
            ChatDelegateRequestMsg::GetRequest {
                key: ChatDelegateKey::new(ROOMS_META_KEY.to_vec()),
            },
        )
        .await
        {
            Ok(ChatDelegateResponseMsg::GetResponse {
                value: Some(bytes), ..
            }) => from_reader::<RoomsMeta, _>(&bytes[..]).ok(),
            _ => None,
        }
    } else {
        None
    };

    if slots.is_empty() {
        return;
    }

    // is_legacy_delegate = true: skip legacy cursor restore, and trigger the
    // re-save to the CURRENT delegate (which also marks legacy migration done).
    let loaded = reconstruct_rooms(slots, meta);
    if hydrate_loaded_rooms(loaded, true) {
        schedule_subscription_timeout_check();
    }
}

/// Build an empty in-memory [`Rooms`]. (`Rooms` deliberately has no `Default`.)
fn empty_rooms() -> Rooms {
    Rooms {
        map: HashMap::new(),
        current_room_key: None,
        removed_rooms: std::collections::HashSet::new(),
        notification_modes: HashMap::new(),
        room_order: Vec::new(),
        migrated_rooms: Vec::new(),
    }
}

/// Schedule the subscription-timeout backstop after a per-room load: a delayed
/// `ProcessRooms` so a room whose PUT/GET response never lands gets retried by
/// `rooms_awaiting_subscription()`. Mirrors the `flags.subscriptions_initiated`
/// branch of the message loop, which the per-room path (running off-loop) can't
/// reach via `flags`.
fn schedule_subscription_timeout_check() {
    let tx = crate::components::app::SYNCHRONIZER
        .read()
        .get_message_sender();
    crate::util::safe_spawn_local(async move {
        crate::util::sleep(std::time::Duration::from_millis(
            super::constants::REPUT_DELAY_MS + 1000,
        ))
        .await;
        if let Err(e) =
            tx.unbounded_send(super::freenet_synchronizer::SynchronizerMessage::ProcessRooms)
        {
            error!("Failed to schedule subscription timeout check: {}", e);
        }
    });
}

/// Hydrate the in-memory `ROOMS` signal from a `Rooms` value assembled from the
/// chat delegate — either the per-room `room:<vk>` keys (the per-room load
/// orchestration) or a single-blob `rooms_data` snapshot (a legacy delegate, or
/// a pre-per-room snapshot under the current delegate). Shared by both load
/// paths so hydration is identical: tombstone filtering, current-room restore,
/// signal merge, secret repopulation, actions_state rebuild, per-room
/// signing-key migration + EnsureRoomSubscription chaining, initial-sync
/// marking, NEEDS_SYNC drive, and the legacy-delegate re-save (which now
/// explodes a single blob into per-room keys).
///
/// Returns `true` if any (non-tombstoned) rooms were loaded, so the caller can
/// schedule the subscription-timeout backstop (the old
/// `flags.subscriptions_initiated`). Holds no `self`/`flags`, so it is callable
/// from a spawned task as well as the synchronous message-loop arm.
fn hydrate_loaded_rooms(loaded_rooms: Rooms, is_legacy_delegate: bool) -> bool {
    // Tombstone filter for all downstream loops.
    // Includes both: (a) tombstones in the
    // incoming loaded_rooms, and (b) tombstones
    // already in the current in-memory ROOMS —
    // because legacy delegates predate the
    // tombstone field, the receiver's set is
    // the authoritative one (freenet/river#247).
    let tombstoned: std::collections::HashSet<ed25519_dalek::VerifyingKey> = {
        let mut t = loaded_rooms.removed_rooms.clone();
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
    let user_has_selected = CURRENT_ROOM.read().owner_key.is_some();
    let saved_key = loaded_rooms.current_room_key;
    let saved_tombstoned = saved_key.map(|k| tombstoned.contains(&k)).unwrap_or(false);
    match decide_current_room_restore(
        saved_key.is_some(),
        saved_tombstoned,
        user_has_selected,
        is_legacy_delegate,
    ) {
        CurrentRoomRestore::Restore => {
            let saved_room_key = saved_key.expect("Restore implies saved_key present");
            info!("Restoring current room selection from delegate");
            crate::util::defer(move || {
                *CURRENT_ROOM.write() = CurrentRoom {
                    owner_key: Some(saved_room_key),
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
        .filter(|(key, _)| !tombstoned.contains(*key))
        .map(|(key, room_data)| {
            let owns_room = room_data.owner_vk == room_data.self_sk.verifying_key();
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
            let contract_id_for_owner: Option<[u8; 32]> = if owns_room {
                Some(**crate::util::owner_vk_to_contract_key(&room_data.owner_vk).id())
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
    info!(
        "Migrating signing keys to delegate for {} rooms",
        signing_keys.len()
    );
    for (room_key, delegate_room_key, signing_key, owner_contract_id) in &signing_keys {
        {
            // Spawn async migration task via
            // `safe_spawn_local`: per AGENTS.md
            // "Dioxus WASM Signal Safety", direct
            // `spawn_local` from inside a polled
            // future causes RefCell re-entrancy
            // panics on Firefox mobile.
            let room_key_copy = *room_key;
            let delegate_room_key = *delegate_room_key;
            let signing_key = signing_key.clone();
            let owner_contract_id = *owner_contract_id;
            crate::util::safe_spawn_local(async move {
                let result =
                    crate::signing::migrate_signing_key(delegate_room_key, &signing_key).await;

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
                if let Some(contract_id) = owner_contract_id {
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
            });
        }
    }

    // Mark all loaded rooms as having completed initial sync
    // and subscribe to receive updates
    for room_key in &room_keys {
        let room_key_copy = *room_key;
        crate::util::defer(move || {
            mark_initial_sync_complete(&room_key_copy);
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
    let had_loaded_rooms = !room_keys.is_empty();
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
        crate::components::app::mark_needs_sync(room_key);
    }
    // The caller schedules the subscription-timeout backstop (formerly
    // `flags.subscriptions_initiated`) based on our `had_loaded_rooms` return.

    // NOTE: legacy-delegate migration is permanent — every delegate-WASM bump
    // If this was from the legacy delegate, save to the new delegate
    if is_legacy_delegate {
        info!("Migrating room data from legacy delegate to new delegate");
        crate::util::safe_spawn_local(async {
            match save_rooms_to_delegate().await {
                Ok(_) => {
                    info!("Successfully migrated room data to new delegate");
                    mark_legacy_migration_done();
                }
                Err(e) => {
                    error!("Failed to migrate room data to new delegate: {}", e);
                    // Don't mark as done - will retry on next startup
                }
            }
        });
    }
    had_loaded_rooms
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

    // ===================================================================
    // Per-room load (freenet/river#345 / #65)
    // ===================================================================

    fn vk(seed: u8) -> ed25519_dalek::VerifyingKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    fn dk(bytes: Vec<u8>) -> ChatDelegateKey {
        ChatDelegateKey::new(bytes)
    }

    /// Per-room keys present → fetch every slot; `rooms_meta` toggles `has_meta`.
    /// A stray legacy `rooms_data` blob alongside per-room keys is ignored (it's
    /// only a rollback fallback once the per-room keys exist).
    #[test]
    fn plan_picks_per_room_when_room_keys_present() {
        let a = vk(1);
        let b = vk(2);
        let keys = vec![
            dk(room_storage_key(&a)),
            dk(ROOMS_STORAGE_KEY.to_vec()), // stray legacy blob — must be ignored
            dk(room_storage_key(&b)),
            dk(ROOMS_META_KEY.to_vec()),
            dk(OUTBOUND_DMS_STORAGE_KEY.to_vec()),
        ];
        match plan_load_from_keys(&keys) {
            LoadPlan::PerRoom { room_vks, has_meta } => {
                assert!(has_meta);
                let set: std::collections::HashSet<_> = room_vks.into_iter().collect();
                assert_eq!(set, std::collections::HashSet::from([a, b]));
            }
            other => panic!("expected PerRoom, got {other:?}"),
        }
    }

    #[test]
    fn plan_per_room_without_meta() {
        let keys = vec![dk(room_storage_key(&vk(9)))];
        assert_eq!(
            plan_load_from_keys(&keys),
            LoadPlan::PerRoom {
                room_vks: vec![vk(9)],
                has_meta: false,
            }
        );
    }

    /// No per-room keys but a single legacy blob → migrate the current blob.
    #[test]
    fn plan_migrates_current_blob_when_only_legacy_blob() {
        let keys = vec![
            dk(ROOMS_STORAGE_KEY.to_vec()),
            dk(OUTBOUND_DMS_STORAGE_KEY.to_vec()),
        ];
        assert_eq!(plan_load_from_keys(&keys), LoadPlan::MigrateCurrentBlob);
    }

    /// Nothing room-related → probe legacy delegates (and the empty-list case).
    #[test]
    fn plan_probes_legacy_when_no_room_data() {
        assert_eq!(
            plan_load_from_keys(&[dk(OUTBOUND_DMS_STORAGE_KEY.to_vec())]),
            LoadPlan::ProbeLegacy
        );
        assert_eq!(plan_load_from_keys(&[]), LoadPlan::ProbeLegacy);
    }

    /// Reconstruct routes `Present` → `map`, `Tombstone` → `removed_rooms`, and
    /// `apply_meta` layers the view prefs (pruning `room_order` to present
    /// rooms). This is the load half of the per-room save/load round-trip.
    #[test]
    fn reconstruct_rooms_splits_present_and_tombstone_and_applies_meta() {
        let present = vk(1);
        let left = vk(2);
        let ghost = vk(3);

        let slots = vec![
            (
                present,
                RoomSlot::Present(Box::new(crate::room_data::test_minimal_room_data(present))),
            ),
            (left, RoomSlot::Tombstone),
        ];
        let meta = RoomsMeta {
            current_room_key: Some(present),
            notification_modes: std::collections::HashMap::new(),
            // `ghost` is not a present room → must be pruned by apply_meta.
            room_order: vec![ghost, present],
        };

        let rooms = reconstruct_rooms(slots, Some(meta));

        assert!(rooms.map.contains_key(&present));
        assert!(!rooms.map.contains_key(&left));
        assert!(rooms.removed_rooms.contains(&left));
        assert_eq!(rooms.current_room_key, Some(present));
        assert_eq!(rooms.room_order, vec![present]);
    }

    /// No meta key → reconstruct still yields the slots, with default view prefs.
    #[test]
    fn reconstruct_rooms_without_meta_keeps_defaults() {
        let a = vk(5);
        let rooms = reconstruct_rooms(
            vec![(
                a,
                RoomSlot::Present(Box::new(crate::room_data::test_minimal_room_data(a))),
            )],
            None,
        );
        assert!(rooms.map.contains_key(&a));
        assert_eq!(rooms.current_room_key, None);
        assert!(rooms.room_order.is_empty());
    }

    /// Single-blob → per-room migration round-trip (the on-node upgrade
    /// contract): a legacy `Rooms` value, when EXPLODED into per-room slots the
    /// way the save path serializes them (`RoomSlot::Present` per room +
    /// `Tombstone` per removed + `to_meta`) and then RECONSTRUCTED the way the
    /// load path does, recovers the same rooms, tombstones, and view prefs — no
    /// room lost across the explosion. Exercises the real CBOR encode/decode at
    /// each per-key boundary (the websocket-driven `migrate_current_blob_to_per_room`
    /// itself isn't unit-testable, but this pins the format it relies on).
    #[test]
    fn single_blob_explodes_and_reconstructs_without_loss() {
        let present_a = vk(11);
        let present_b = vk(12);
        let left = vk(13);

        // A legacy single-blob Rooms with 2 present rooms, 1 tombstone, and prefs.
        let mut blob = empty_rooms();
        blob.map.insert(
            present_a,
            crate::room_data::test_minimal_room_data(present_a),
        );
        blob.map.insert(
            present_b,
            crate::room_data::test_minimal_room_data(present_b),
        );
        blob.removed_rooms.insert(left);
        blob.current_room_key = Some(present_b);
        blob.room_order = vec![present_b, present_a];

        // Explode exactly as do_save_rooms_to_delegate serializes each key, then
        // decode each back exactly as the load path does.
        let mut slots: Vec<(ed25519_dalek::VerifyingKey, RoomSlot)> = Vec::new();
        for (vk, room) in blob.map.iter() {
            let mut b = Vec::new();
            ciborium::ser::into_writer(&RoomSlot::Present(Box::new(room.clone())), &mut b).unwrap();
            let decoded: RoomSlot = from_reader(b.as_slice()).unwrap();
            slots.push((*vk, decoded));
        }
        for vk in blob.removed_rooms.iter() {
            let mut b = Vec::new();
            ciborium::ser::into_writer(&RoomSlot::Tombstone, &mut b).unwrap();
            let decoded: RoomSlot = from_reader(b.as_slice()).unwrap();
            slots.push((*vk, decoded));
        }
        let mut mb = Vec::new();
        ciborium::ser::into_writer(&blob.to_meta(), &mut mb).unwrap();
        let meta: RoomsMeta = from_reader(mb.as_slice()).unwrap();

        let recovered = reconstruct_rooms(slots, Some(meta));

        assert!(recovered.map.contains_key(&present_a));
        assert!(recovered.map.contains_key(&present_b));
        assert!(!recovered.map.contains_key(&left));
        assert!(recovered.removed_rooms.contains(&left));
        assert_eq!(recovered.current_room_key, Some(present_b));
        assert_eq!(recovered.room_order, vec![present_b, present_a]);
    }
}
