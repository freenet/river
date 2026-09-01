// `pub(crate)` so the backward-probe adapter (freenet_api::backward_probe) can
// call `get_response::{adopt_recovered_probe_state, seed_current_key_with_local,
// merge_room_states}` when its decision driver reaches a terminal outcome (#292,
// #398 phase 2).
pub(crate) mod get_response;
mod put_response;
mod subscribe_response;
mod update_notification;
mod update_response;

use super::error::SynchronizerError;
use super::room_synchronizer::RoomSynchronizer;
use crate::components::app::chat_delegate::{
    arm_legacy_migration_recovery, await_delegate_response, clear_legacy_migration_in_progress,
    complete_pending_public_key_request, complete_pending_request, complete_pending_sign_request,
    complete_pending_signing_key_request, current_delegate_source_rank,
    decide_legacy_migration_action, decide_per_room_load_action, enqueue_delegate_request,
    fire_legacy_migration_request, hydrate_hidden_dm_threads, hydrate_outbound_dms_cache,
    is_legacy_delegate_key, is_legacy_migration_in_progress, legacy_scoped_correlation,
    load_state_after_probe_legacy, mark_legacy_migration_done, mark_legacy_migration_in_progress,
    parse_room_storage_key, per_room_terminal, prune_outbound_dms_for_purges,
    request_legacy_seal_on_quiescence, response_correlation_base, room_storage_key,
    save_outbound_dms_to_delegate, save_rooms_to_delegate, send_delegate_request,
    send_delegate_request_to, set_load_state_if_current, source_rank_for_delegate_key,
    LegacyMigrationAction, LoadWorkerGuard, PendingDelegateRequest, RoomsLoadState,
    OUTBOUND_DMS_STORAGE_KEY, ROOMS_META_KEY, ROOMS_STORAGE_KEY,
};
use crate::components::app::document_title::{mark_current_room_as_read, update_document_title};
use crate::components::app::notifications::mark_initial_sync_complete;
use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::room_data::CurrentRoom;
use crate::room_data::{RoomSlot, Rooms, RoomsMeta};
use crate::util::ecies::decrypt_with_symmetric_key;
use ciborium::de::from_reader;
use dioxus::logger::tracing::{debug, error, info, warn};
use dioxus::prelude::ReadableExt;

use freenet_stdlib::client_api::{ContractResponse, HostResponse};
use freenet_stdlib::prelude::{DelegateKey, OutboundDelegateMsg};
pub use get_response::handle_get_response;
pub use put_response::handle_put_response;
use river_core::chat_delegate::{
    CasStoreResult, ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg,
    OutboundDmStore,
};
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
                // Which delegate GENERATION answered (freenet/river#527).
                // Captured here, at the DelegateResponse scope, because the
                // per-value match below shadows `key` with the storage key.
                let delegate_source_rank = source_rank_for_delegate_key(key.bytes());
                if is_legacy_delegate {
                    info!("Received response from LEGACY delegate - checking for migration data");
                }

                // Log a short key fingerprint, not the full `DelegateKey` Debug: that
                // prints the 32-byte key as a decimal byte array (~450 B a line, on
                // every delegate response). freenet/river#569.
                info!(
                    "Received delegate response from API with key: {} containing {} values",
                    delegate_key_fp(&key),
                    values.len()
                );
                for (i, v) in values.iter().enumerate() {
                    // Per-response developer tracing, not operator-facing: `debug!`
                    // compiles out in release (`release_max_level_info`), which removes
                    // the per-CALL cost as well as the per-byte cost. freenet/river#569.
                    debug!("Processing delegate response value #{}", i);
                    match v {
                        OutboundDelegateMsg::ApplicationMessage(app_msg) => {
                            debug!(
                                "Delegate response is an ApplicationMessage, processed flag: {}",
                                app_msg.processed
                            );

                            // Size only. This used to format up to 100 payload bytes as a
                            // decimal array on every delegate response; the byte values
                            // were never actually used for triage, the size and the
                            // decoded variant below are. freenet/river#569.
                            info!(
                                "ApplicationMessage payload: {} bytes",
                                app_msg.payload.len()
                            );

                            // Try to deserialize as a response
                            let deserialization_result = from_reader::<ChatDelegateResponseMsg, _>(
                                app_msg.payload.as_slice(),
                            );

                            // Also try to deserialize as a request to see if that's what's happening
                            let request_deser_result = from_reader::<ChatDelegateRequestMsg, _>(
                                app_msg.payload.as_slice(),
                            );
                            debug!(
                                "Deserialization as request result: {:?}",
                                request_deser_result.is_ok()
                            );

                            if let Ok(response) = deserialization_result {
                                // Summary, NOT the full Debug. This single call site was
                                // 98.5% of all console bytes River emits during startup:
                                // 39 lines averaging ~383 KB (14.93 MB of 15.15 MB total).
                                //
                                // EVERY response variant logs here, and the big ones are
                                // whichever carry a blob: startup issues one GetRequest per
                                // room, so ~39 `GetResponse`s each Debug-printing a
                                // per-room blob (~343 KB in memory, ~4x that as a decimal
                                // array) is the best fit for the line count and size.
                                // `ListResponse` key arrays land here too. Do not read the
                                // count as attributed to one variant -- the census grouped
                                // by source line, not by variant. freenet/river#569.
                                info!(
                                    "Successfully deserialized as ChatDelegateResponseMsg: {}",
                                    describe_delegate_response(&response)
                                );

                                // For a response from a LEGACY delegate, the awaiting request
                                // registered under a delegate-scoped correlation key (so a
                                // legacy read of room:A can't collide with a current/other-legacy
                                // read of room:A — codex/skeptical re-review). Rebuild that same
                                // scoped key from the responding delegate. Current-delegate
                                // responses use the bare correlation (unchanged).
                                let scoped =
                                    |base: Vec<u8>| -> river_core::chat_delegate::ChatDelegateKey {
                                        river_core::chat_delegate::ChatDelegateKey::new(
                                            if is_legacy_delegate {
                                                legacy_scoped_correlation(&key, &base)
                                            } else {
                                                base
                                            },
                                        )
                                    };

                                // Try to complete any pending request waiting for this response.
                                //
                                // Every storage response derives its correlation base from the
                                // SINGLE shared `response_correlation_base` (the mirror of
                                // `get_request_key`) and is scoped HERE, in one place. It used
                                // to be six hand-written per-variant arms, and `ListResponse`
                                // was the one that forgot `scoped()` — leaving every legacy
                                // `ListRequest` waiter uncompletable and the crate walk dead.
                                // Do NOT reintroduce per-variant key construction: a variant
                                // that builds its own key can silently skip the scoping again.
                                let completed = match &response {
                                    ChatDelegateResponseMsg::GetResponse { .. }
                                    | ChatDelegateResponseMsg::StoreResponse { .. }
                                    | ChatDelegateResponseMsg::DeleteResponse { .. }
                                    | ChatDelegateResponseMsg::GetVersionedResponse { .. }
                                    | ChatDelegateResponseMsg::CasStoreResponse { .. }
                                    | ChatDelegateResponseMsg::ListResponse { .. } => {
                                        match response_correlation_base(&response) {
                                            Some(base) => complete_pending_request(
                                                &scoped(base),
                                                response.clone(),
                                            ),
                                            None => false,
                                        }
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
                                                // Progress-tracked termination
                                                // (freenet/river#397 review 6/7):
                                                // the legacy single-blob response
                                                // is a load worker. The guard wraps
                                                // BOTH parse arms so the Err arm's
                                                // failure is attempt-scoped AND its
                                                // settlement resolves `LoadFailed`
                                                // even if it arrives AFTER the idle
                                                // timer already set Empty (P2#2).
                                                let worker = LoadWorkerGuard::new();
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
                                                                    // SYNCHRONOUS, deliberately. This door is not a
                                                                    // migration completion — the current delegate is
                                                                    // authoritative and its sibling arm is the one that
                                                                    // fires the fan-out, so there are no probes in
                                                                    // flight to wait for. Deferring it behind the
                                                                    // debounce would buy nothing and would weaken the
                                                                    // freenet/river#253 guard, since a reconnect inside
                                                                    // the window cancels the pending seal and could let
                                                                    // the fan-out fire for a user who HAS data.
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
                                                            delegate_source_rank,
                                                            worker.attempt(),
                                                        ) {
                                                            flags.subscriptions_initiated = true;
                                                        }
                                                    }
                                                    Err(e) => {
                                                        error!(
                                                            "Failed to deserialize rooms data: {}",
                                                            e
                                                        );
                                                        // freenet/river#397 review 5/7: a
                                                        // `rooms_data` blob exists only because
                                                        // rooms were stored in it. This arm is
                                                        // reachable only for a LEGACY delegate (the
                                                        // current-delegate flow is List-driven), so
                                                        // a parse failure is known-stored legacy
                                                        // data that failed to load. Attempt-scoped
                                                        // mark; the guard's settlement (on drop)
                                                        // resolves `LoadFailed` — correcting a
                                                        // premature Empty from a late response
                                                        // (P2#2). (A genuine new user has no legacy
                                                        // delegate responding, so never reaches
                                                        // here.)
                                                        worker.mark_fetch_failure();
                                                    }
                                                }
                                            } else {
                                                info!("No rooms data found in delegate");
                                                if is_legacy_delegate {
                                                    // A legacy delegate with NO `rooms_data` blob is
                                                    // NOT necessarily empty: since freenet/river#345
                                                    // rooms are stored under dynamic `room:<vk>` keys,
                                                    // which this blob response can't see. Do NOT mark
                                                    // migration done here — that would permanently
                                                    // strand the per-room keys if the List-driven
                                                    // `migrate_legacy_per_room` then fails (the exact
                                                    // V24→V25 data-loss the code-first/skeptical
                                                    // re-review caught). Done-marking is owned by the
                                                    // successful-migration paths: `hydrate_loaded_rooms`'s
                                                    // legacy re-save (rooms_data-has-data and per-room
                                                    // alike) and the current-delegate PerRoom load. A
                                                    // genuinely-empty legacy delegate simply gets
                                                    // re-probed next session (harmless, fire-and-forget,
                                                    // guarded within-session by LEGACY_MIGRATION_ATTEMPTED).
                                                    info!(
                                                        "Legacy delegate has no rooms_data blob; per-room keys (if any) are handled by migrate_legacy_per_room"
                                                    );
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
                                        } else if crate::components::app::freenet_api::delegate_migration::is_migration_marker_key(
                                            key.as_bytes(),
                                        ) {
                                            // Crate-walk per-predecessor markers. Like the
                                            // per-room keys above, these are consumed by the
                                            // awaiting walk via the pending-request registry;
                                            // the processing match still runs for every
                                            // response. Without this branch a single walk
                                            // logged ~54 "Unexpected key" warnings per page
                                            // load — noise that trains the reader to ignore a
                                            // warning that is meant to indicate a real gap.
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
                                            // Suppresses the DUPLICATE migration when the crate
                                            // walk is also probing this delegate. Two concurrent
                                            // `migrate_legacy_per_room` runs share the walk's
                                            // scoped correlation keys, so they displace each
                                            // other in the single-waiter map — `Cancelled` reads,
                                            // and a possible false `LoadFailed` for a user whose
                                            // migration actually SUCCEEDED.
                                            //
                                            // Be precise about WHY this is sufficient, because
                                            // the obvious reason is the wrong one. Responses bind
                                            // to waiters by CORRELATION KEY, not by which send
                                            // produced them: the sweep's `ListRequest` and the
                                            // walk's go to the same delegate under the same
                                            // scoped key, so the sweep's response can perfectly
                                            // well complete the WALK's waiter. Which response
                                            // sees `completed == true` is interleaving-dependent.
                                            //
                                            // What makes it safe is a COUNTING argument: the
                                            // sweep's probe registers no waiter (it is
                                            // fire-and-forget), so N responses meet at most N-1
                                            // walk waiters, and at least one response therefore
                                            // falls through to spawn the migration exactly once.
                                            //
                                            // Two reachable exceptions, both fail-safe and both
                                            // recovered by the durable markers on the next page
                                            // load, neither losing data:
                                            //  * a walk waiter times out (10 s) and is evicted
                                            //    before its response lands — the late response
                                            //    then spawns a migration that can overlap the
                                            //    sweep-driven one;
                                            //  * the sweep's send fails while the walk's
                                            //    succeeds — the walk consumes the only response
                                            //    and no sweep migration spawns this load.
                                            if !completed {
                                                let legacy_key = key.clone();
                                                crate::util::safe_spawn_local(async move {
                                                    migrate_legacy_per_room(legacy_key, keys).await;
                                                });
                                            }
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
                                // The ONLY path where raw bytes still earn their
                                // keep: the payload did not decode, so no variant
                                // summary exists and the size alone cannot tell a
                                // wire-format break from a corrupt frame. A short
                                // HEX head is enough to identify the CBOR shape from
                                // a user's console paste, and costs nothing on the
                                // happy path because this branch is the failure
                                // branch. See bug-prevention-patterns.md, "new enum
                                // variants cause deserialization failures in old
                                // clients" (the v0.2.11 incident).
                                let head: String = app_msg
                                    .payload
                                    .iter()
                                    .take(24)
                                    .map(|b| format!("{b:02x}"))
                                    .collect();
                                warn!(
                                    "Failed to deserialize chat delegate response \
                                     ({} bytes, head {})",
                                    app_msg.payload.len(),
                                    head
                                );
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

/// How many per-room delegate GETs to keep in flight at once during the startup
/// room load (freenet/river#417 + PR #419 review).
///
/// The concurrent fan-out collapses N serial round-trips into ~ceil(N / this),
/// but MUST stay well under the node's per-queue admission cap: delegate
/// requests land in the freenet-core `FairEventQueue` `Default` queue, whose
/// per-tier cap is `MAX_QUEUED_PER_CONTRACT` (100), and admission past it is
/// REJECTED (the waiter then fails / times out, failing that room's load). 16
/// leaves comfortable headroom for the cap AND for the other startup delegate
/// traffic sharing that queue (RegisterDelegate, the outbound-DM load, signing
/// requests), while still turning a typical (<16-room) account's load into a
/// single wave. Waves run sequentially, so each wave also gets a fresh response
/// timeout — a slow first-time delegate WASM compile can't time out later rooms.
const ROOM_LOAD_CONCURRENCY: usize = 16;
// `slice::chunks(0)` panics; pin the wave size as a valid chunk at compile time.
const _: () = assert!(ROOM_LOAD_CONCURRENCY > 0);

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
///
/// The per-room slot GETs are fanned out CONCURRENTLY in waves of
/// [`ROOM_LOAD_CONCURRENCY`] (freenet/river#417) to collapse N serial delegate
/// round-trips into ~ceil(N / ROOM_LOAD_CONCURRENCY).
async fn load_rooms_per_room(keys: Vec<ChatDelegateKey>) {
    // Progress-tracked termination (freenet/river#397 review 6): this awaited
    // worker holds a guard for its whole lifetime, so on EVERY exit path the
    // settled-handler re-evaluates whether the load is complete. For the
    // ProbeLegacy case the guard drop is exactly the "fired the fire-and-forget
    // probe and became quiescent" point that arms the idle timer.
    //
    // Attempt-scoped (review 7): every global write below goes through
    // `worker.mark_fetch_failure()` / `worker.set_load_state()` so a stale worker
    // (superseded by a Retry) can neither pollute the new attempt's
    // `SAW_FETCH_FAILURE` nor write its display state.
    let worker = LoadWorkerGuard::new();
    match plan_load_from_keys(&keys) {
        LoadPlan::ProbeLegacy => {
            info!("Current delegate has no room data — firing legacy migration probe");
            // freenet/river#397: stay `Loading` while a dispatched fire-and-forget
            // probe might still yield a migration; resolve to `Loaded` only if the
            // probe was skipped (nothing is coming — the fast path for a returning
            // empty user). The universal backstop (armed in `set_up_chat_delegate`)
            // guarantees termination if the probe finds nothing.
            let fired = fire_legacy_migration_request().await;
            worker.set_load_state(load_state_after_probe_legacy(fired));
        }
        LoadPlan::MigrateCurrentBlob => {
            info!(
                "No per-room keys but a current-delegate rooms_data blob exists — \
                 migrating it to per-room keys"
            );
            // Initial load: owns the display state (recovery = false).
            migrate_current_blob_to_per_room(false).await;
        }
        LoadPlan::PerRoom {
            mut room_vks,
            has_meta,
        } => {
            // Was a prior legacy migration interrupted before it finished writing
            // every per-room key? If so, the set we're about to load may be
            // missing rooms the legacy delegate still holds, and we must NOT mark
            // migration done — we re-run the legacy fill below to recover them
            // (freenet/river#345 follow-up). Otherwise the per-room set is
            // authoritative: mark done so we never probe legacy (#253 — a stale
            // legacy response could clobber newer state). The decision is a pure,
            // unit-tested function (`decide_per_room_load_action`).
            let migration_interrupted = is_legacy_migration_in_progress();
            let action = decide_per_room_load_action(migration_interrupted);
            if action.mark_done {
                // SYNCHRONOUS, deliberately — see the rooms_data door above.
                // The per-room set being authoritative is precisely the
                // freenet/river#253 condition under which legacy must never be
                // probed; a deferred seal here is cancellable by a reconnect and
                // would re-open that. When this fires, `migration_interrupted`
                // is false, so no recovery re-probe is pending either.
                mark_legacy_migration_done();
            }

            // Defensive dedup (PR #419 review): the concurrent fan-out registers
            // every per-room waiter under its bare `room_storage_key` in the
            // single-waiter `PENDING_REQUESTS` map, so a duplicate `vk` would
            // clobber its own sibling waiter (the second register drops the
            // first's oneshot) and spuriously mark a fetch failure for a room that
            // actually loaded. The delegate's key index is unique by construction,
            // but the OLD serial loop was robust to a dup (each GET completed
            // before the next registered) and the fan-out is not — so restore that
            // robustness cheaply. Order-preserving.
            {
                let mut seen = std::collections::HashSet::with_capacity(room_vks.len());
                room_vks.retain(|vk| seen.insert(vk.to_bytes()));
            }

            // Concurrency note: a WebSocket reconnect re-fires `ListRequest`, so
            // two `load_rooms_per_room` tasks can briefly overlap. The per-room
            // GETs register under bare-key correlation in the single-waiter
            // `PENDING_REQUESTS` map, so the second (newer) load's GET for a shared
            // key orphans the first's oneshot. With the concurrent fan-out below,
            // a whole wave of the older pass's waiters can be orphaned at once
            // (vs. one-at-a-time in the old serial loop). This stays safe because
            // the reconnect goes through `set_up_chat_delegate()`, which calls
            // `begin_load_attempt()` (bumping `LOAD_ATTEMPT_GEN`) BEFORE firing the
            // new `ListRequest` — so by the time the newer pass can orphan the
            // older pass's waiters, the older pass is already superseded and its
            // `worker.set_load_state(...)` / `worker.mark_fetch_failure()` writes
            // no-op (`load_attempt_is_current`). No permanent loss and no false
            // `LoadFailed` flash: the newer pass loads every room and `Rooms::merge`
            // is a union. (Load-vs-SAVE never collides — distinct prefixed
            // correlation keys.)
            info!("Loading {} per-room slot(s) from delegate", room_vks.len());
            // freenet/river#397 Codex review 4: the index PROVED these rooms
            // exist. Track whether any LISTED room failed to materialize (a
            // transport / parse error — NOT a definitive `value: None`) so the
            // authoritative PerRoom terminal can resolve to `LoadFailed` instead
            // of a false "no rooms yet". Set the global `SAW_FETCH_FAILURE` too so
            // the backstop (and concurrent legacy probes) see it.
            let listed_count = room_vks.len();
            let mut had_fetch_error = false;
            let mut slots: Vec<(ed25519_dalek::VerifyingKey, RoomSlot)> = Vec::new();
            // Bounded concurrency (freenet/river#417 + PR #419 review): fan the
            // per-room GETs out in WAVES of `ROOM_LOAD_CONCURRENCY`. Firing all N
            // at once would collapse N round-trips into 1, but a large account (or
            // a node whose delegate queue is already busy) could overflow the
            // node's `FairEventQueue` `Default`-queue cap and get rooms rejected —
            // a regression the serial loop couldn't hit (see `ROOM_LOAD_CONCURRENCY`).
            // Waves keep in-flight requests under that cap while still collapsing
            // N serial round-trips into ~ceil(N / ROOM_LOAD_CONCURRENCY).
            //
            // Within a wave, every request is ENQUEUED (the synchronous WS send is
            // the only `WEB_API` borrow, kept strictly non-overlapping) BEFORE any
            // response is awaited via `join_all` — so no second `WEB_API` borrow
            // can interleave across an await (a `RefCell` double-borrow panic in
            // single-threaded WASM). An enqueue (send) failure is a transport
            // failure, funneled into the per-room `Err` fold arm below so there is
            // a single per-room failure-handling site.
            let mut per_room_responses: Vec<(
                ed25519_dalek::VerifyingKey,
                Result<ChatDelegateResponseMsg, String>,
            )> = Vec::with_capacity(listed_count);
            for wave in room_vks.chunks(ROOM_LOAD_CONCURRENCY) {
                let mut pending_slots: Vec<(ed25519_dalek::VerifyingKey, PendingDelegateRequest)> =
                    Vec::with_capacity(wave.len());
                for vk in wave {
                    match enqueue_delegate_request(ChatDelegateRequestMsg::GetRequest {
                        key: ChatDelegateKey::new(room_storage_key(vk)),
                    })
                    .await
                    {
                        Ok(pending) => pending_slots.push((*vk, pending)),
                        // The WS send failed (connection down): record as a
                        // per-room `Err` so it flows through the single failure
                        // arm below.
                        Err(e) => per_room_responses.push((*vk, Err(e))),
                    }
                }
                // Await this wave's responses concurrently — ~1 round-trip per
                // wave instead of one per room.
                per_room_responses.extend(
                    futures::future::join_all(pending_slots.into_iter().map(
                        |(vk, pending)| async move { (vk, await_delegate_response(pending).await) },
                    ))
                    .await,
                );
            }
            for (vk, response) in per_room_responses {
                match response {
                    Ok(ChatDelegateResponseMsg::GetResponse {
                        value: Some(bytes), ..
                    }) => match from_reader::<RoomSlot, _>(&bytes[..]) {
                        Ok(slot) => slots.push((vk, slot)),
                        Err(e) => {
                            // A listed room whose slot won't parse failed to
                            // materialize — a fetch failure.
                            error!("Unparseable room slot for {:?}: {}", vk, e);
                            had_fetch_error = true;
                            worker.mark_fetch_failure();
                        }
                    },
                    Ok(ChatDelegateResponseMsg::GetResponse { value: None, .. }) => {
                        // Listed in the index but the value is gone — a DEFINITIVE
                        // "no value", a legitimate skip, NOT a fetch failure.
                        warn!("Room key {:?} listed but value missing", vk);
                    }
                    Ok(other) => {
                        error!("Unexpected response loading room {:?}: {:?}", vk, other);
                        had_fetch_error = true;
                        worker.mark_fetch_failure();
                    }
                    Err(e) => {
                        error!("Failed to load room {:?}: {}", vk, e);
                        had_fetch_error = true;
                        worker.mark_fetch_failure();
                    }
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
                        // freenet/river#397 Codex review 9 (send-side audit): a
                        // rooms_meta send returning Err is a transport failure
                        // (connection down — the per-room slot GETs on the same
                        // connection would fail too). Mark it so the load resolves
                        // to LoadFailed rather than a silent Empty.
                        error!("Failed to load rooms_meta: {}", e);
                        worker.mark_fetch_failure();
                        None
                    }
                }
            } else {
                None
            };

            let loaded = reconstruct_rooms(slots, meta);
            // Capture emptiness BEFORE `loaded` is moved into hydrate, for the
            // authoritative terminal decision below.
            let loaded_map_empty = loaded.map.is_empty();
            if hydrate_loaded_rooms(
                loaded,
                false,
                current_delegate_source_rank(),
                worker.attempt(),
            ) {
                schedule_subscription_timeout_check();
            }

            // freenet/river#397 (Codex review 4): the current delegate's per-room
            // set is the authoritative initial load — a definitive fast-path
            // completion. Resolve via the pure `per_room_terminal`: rooms present →
            // `Loaded` (List); empty with a known-rooms fetch failure → `LoadFailed`
            // (never a false "no rooms yet"); a genuine empty (nothing listed, or
            // every slot a clean `value: None`/tombstone) → `Loaded` (Empty). Set
            // BEFORE the recovery re-run below so a recovery that finds stranded
            // rooms and sets `Migrating` wins over this terminal.
            worker.set_load_state(per_room_terminal(
                loaded_map_empty,
                listed_count,
                had_fetch_error,
            ));

            // Recover from an interrupted migration by re-running it to pick up
            // any room whose per-room key wasn't written before the previous
            // attempt was cut short. `migrate_current_blob_to_per_room`
            // dispatches to whichever source applies — a current-delegate
            // `rooms_data` blob (re-explode) or, when there's none (the common
            // case), the legacy delegates via `fire_legacy_migration_request`.
            //
            // Bounds & safety:
            // - `arm_legacy_migration_recovery()` fires this at most ONCE per
            //   session AND clears the per-session `LEGACY_MIGRATION_ATTEMPTED`
            //   guard, so the probe isn't a silent no-op when the initial
            //   migration this session already set that guard (codex review of
            //   #352). Without the reset, same-session recovery (a CAS failure
            //   mid-save, then a reconnect) wouldn't fire until a page reload.
            // - The re-save is per-room CAS read-merge-write: a room already
            //   present merges (CRDT union) rather than being overwritten, so
            //   recovering an already-present, same-identity room neither loses
            //   messages nor clobbers newer state. (The narrow diverged-identity
            //   case — same room key under a different signing key — keeps the
            //   local copy per `reconcile_room_present`; recovery does not widen
            //   that pre-existing behavior.)
            // - On a successful full re-save the in-progress flag is cleared and
            //   migration is marked done, so recovery converges. If the legacy
            //   delegate is genuinely empty or no longer installed, nothing is
            //   recovered and the flag persists; the next session re-probes once
            //   — the SAME bounded, harmless cost the existing empty-legacy path
            //   already pays (see the `is_legacy_delegate` no-`rooms_data` branch
            //   above). No data loss, no spam loop.
            if action.recover && arm_legacy_migration_recovery() {
                info!("Prior migration was interrupted — re-running to recover any stranded rooms");
                // Background re-fill: the initial per-room load already resolved
                // the display state to `Loaded` above, so recovery must NOT write
                // the load state (recovery = true) — see freenet/river#397 review.
                migrate_current_blob_to_per_room(true).await;
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
/// `recovery` distinguishes the two callers (freenet/river#397 review): the
/// initial `LoadPlan::MigrateCurrentBlob` load (`false`) OWNS the display state,
/// so it drives `ROOMS_LOAD_STATE`; the interrupted-migration recovery re-run
/// (`true`) fires AFTER the per-room load already resolved the state to `Loaded`,
/// so it must be a pure background re-fill that NEVER writes the load state — in
/// particular it must never push a resolved `Loaded` back to `Loading` (which
/// its no-current-blob `fire_legacy_migration_request` path would otherwise do,
/// because `arm_legacy_migration_recovery` reset the attempt guard so the probe
/// re-fires). Any real migration the recovery triggers still shows `Migrating`
/// via the legacy `hydrate_loaded_rooms` path, which is not gated here.
async fn migrate_current_blob_to_per_room(recovery: bool) {
    // Progress-tracked termination (freenet/river#397 review 6); attempt-scoped
    // writes (review 7): a stale worker's `worker.*` calls no-op.
    let worker = LoadWorkerGuard::new();
    match send_delegate_request(ChatDelegateRequestMsg::GetVersionedRequest {
        key: ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
    })
    .await
    {
        Ok(ChatDelegateResponseMsg::GetVersionedResponse {
            value: Some(bytes), ..
        }) => match from_reader::<Rooms, _>(&bytes[..]) {
            Ok(loaded) => {
                // Mark in progress BEFORE the explosion save; only mark done once
                // it fully succeeds — otherwise a partial explosion would strand
                // rooms on the next per-room load (freenet/river#345 follow-up,
                // same hazard as the legacy-delegate path).
                mark_legacy_migration_in_progress();
                // freenet/river#397: a current-delegate blob explosion is a
                // migration. Set `Migrating` BEFORE hydrate merges the rooms so a
                // re-render lands on the "Migrating…" state before the list fills.
                // Suppressed under `recovery` (the state is already `Loaded`).
                if !recovery {
                    worker.set_load_state(RoomsLoadState::Migrating);
                }
                // Did the blob carry any LIVE rooms (vs. all tombstones/empty)?
                // freenet/river#397 Codex review 4: only a completion that merged
                // live rooms writes `Loaded` — a zero-live-room completion writes
                // NOTHING so it can't stomp a concurrent legacy probe's `Migrating`
                // into a false Empty; the universal backstop owns that terminal.
                // freenet/river#590: the blob is a frozen SNAPSHOT that lives on
                // the current delegate — not the live current delegate, and not a
                // legacy delegate either. Under the interrupted-migration
                // recovery the per-room load has already merged live rooms, and
                // the blob deliberately survives alongside the per-room keys as a
                // rollback fallback, so a tombstone it carries for a room the user
                // has since rejoined would evict the live room and re-save the
                // tombstone. `OlderSnapshot` at the current delegate's own rank
                // ties with those rooms, and a tie does not evict.
                let had_rooms = hydrate_loaded_rooms_with_authority(
                    loaded,
                    false,
                    current_delegate_source_rank(),
                    crate::room_data::MergeAuthority::OlderSnapshot,
                    worker.attempt(),
                );
                if had_rooms {
                    schedule_subscription_timeout_check();
                }
                // Explode into per-room keys (non-destructive: blob stays).
                // hydrate defers the ROOMS merge via `defer()` (setTimeout 0),
                // so the save MUST also be deferred — a direct `.await` here
                // reads the pre-merge (empty) ROOMS and would write nothing.
                // safe_spawn_local schedules a setTimeout(0) that, being queued
                // AFTER the merge's defer, runs once the merge has populated
                // ROOMS (FIFO ordering), mirroring the legacy-delegate re-save.
                //
                // Review 11: this SPAWNED closure runs after the worker's sync body
                // (its guard may have dropped), so capture the worker's attempt and
                // gate the Loaded writes on still-current explicitly.
                let attempt = worker.attempt();
                crate::util::safe_spawn_local(async move {
                    match save_rooms_to_delegate().await {
                        Ok(_) => {
                            clear_legacy_migration_in_progress();
                            request_legacy_seal_on_quiescence();
                            // Resolve to `Loaded` only if we merged live rooms;
                            // otherwise let the backstop own the terminal.
                            if had_rooms {
                                set_load_state_if_current(attempt, RoomsLoadState::Loaded);
                            }
                        }
                        Err(e) => {
                            error!("Failed to explode single blob into per-room keys: {}", e);
                            // Leave the in-progress flag set so the next load
                            // re-runs the fill. Same had_rooms gate as the Ok arm.
                            if had_rooms {
                                set_load_state_if_current(attempt, RoomsLoadState::Loaded);
                            }
                        }
                    }
                });
            }
            // A corrupt current-delegate blob is unparseable. We intentionally do
            // NOT mark migration done (a corrupt blob is not proof there's nothing
            // to migrate) and do NOT touch the in-progress flag — if a prior
            // migration left it set, the next session re-runs recovery, and the
            // already-loaded per-room keys remain intact.
            Err(e) => {
                error!("Failed to deserialize current-delegate rooms blob: {}", e);
                // freenet/river#397 Codex review 5 (P2b): a `rooms_data` blob only
                // exists because rooms were written to it, so a parse failure is
                // known-stored-data that failed to load — surface LoadFailed (with
                // Retry), NOT a false Empty. Attempt-scoped (review 7) so a stale
                // worker doesn't corrupt the retry.
                worker.mark_fetch_failure();
                worker.set_load_state(RoomsLoadState::LoadFailed);
            }
        },
        Ok(_) => {
            // Index listed rooms_data but the value is gone — treat as empty.
            let fired = fire_legacy_migration_request().await;
            // The STATE write stays gated on `!recovery` — this is the ONE
            // DOWNGRADE write in this function: under recovery,
            // `load_state_after_probe_legacy(true)` would push a resolved `Loaded`
            // (the non-empty-map recovery case) back to `Loading`. On the initial
            // load it is a legitimate fast-path (`Loaded` when the probe was
            // skipped, `Loading` when it fired). The universal backstop (armed in
            // `set_up_chat_delegate`) guarantees termination for the fired case.
            if !recovery {
                worker.set_load_state(load_state_after_probe_legacy(fired));
            }
        }
        Err(e) => {
            // freenet/river#397 Codex review 5 (audit): we only reach this fn from
            // `LoadPlan::MigrateCurrentBlob`, i.e. the current delegate's index
            // LISTED a `rooms_data` key, so the blob exists — a SEND failure
            // reading it is known-stored-data that failed to load. Mark the failure
            // (attempt-scoped, review 7) so the worker's settlement resolves to
            // LoadFailed rather than a silent Empty. (Display is masked by `List`
            // if rooms are concurrently present.)
            warn!("Failed to read current-delegate rooms blob: {}", e);
            worker.mark_fetch_failure();
        }
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
    // Progress-tracked termination (freenet/river#397 review 6): a legacy probe
    // that responded is an active worker. Holding the guard for the whole
    // function (including the empty-index early return) keeps the load alive
    // while this delegate is being processed, and cancels any pending idle
    // resolution (activity-gen bump) so a concurrent empty completion can't
    // resolve Empty while this one may still bring rooms. Attempt-scoped writes
    // (review 7): `worker.*` calls no-op for a stale worker.
    let worker = LoadWorkerGuard::new();
    let (room_vks, has_meta) = match plan_load_from_keys(&keys) {
        LoadPlan::PerRoom { room_vks, has_meta } => (room_vks, has_meta),
        // No per-room keys on this legacy delegate — its single blob (if any) is
        // handled by the fixed rooms_data probe. Nothing per-room to migrate, so
        // do NOT touch the load state here (freenet/river#397 Codex review 2: an
        // EMPTY legacy index must not show Migrating).
        LoadPlan::MigrateCurrentBlob | LoadPlan::ProbeLegacy => return,
    };

    // freenet/river#397 Codex review 2/4 (P1): the legacy index is NON-EMPTY, so
    // rooms provably exist under the old delegate key. Announce `Migrating`
    // BEFORE the sequential fetch loop below — otherwise the state stays
    // `Loading` through the whole fetch window and the 60s backstop could flip it
    // to a terminal despite that positive evidence. `Migrating` is not rescued
    // early by the backstop. On an all-empty/all-failed result this writes NOTHING
    // to the global state (a single legacy probe is not authoritative — concurrent
    // probes may hold rooms); the universal backstop owns that terminal, and any
    // failed legacy fetch below sets `SAW_FETCH_FAILURE` so the backstop resolves
    // to `LoadFailed` rather than a false Empty.
    worker.set_load_state(RoomsLoadState::Migrating);

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
                Err(e) => {
                    error!("Unparseable legacy room slot for {:?}: {}", vk, e);
                    worker.mark_fetch_failure();
                }
            },
            Ok(ChatDelegateResponseMsg::GetResponse { value: None, .. }) => {
                // Definitive "no value for this key" — a legitimate skip, NOT a
                // fetch failure.
                warn!("Legacy room key {:?} listed but value missing", vk);
            }
            Ok(other) => {
                // An unexpected response variant (e.g. a delegate protocol
                // mismatch) is a fetch failure, NOT a definitive missing value —
                // mirror the current-delegate per-room path (review 10 P2#2), or a
                // listed room silently fails to materialize and the idle resolver
                // shows a false Empty.
                error!(
                    "Unexpected response loading legacy room {:?}: {:?}",
                    vk, other
                );
                worker.mark_fetch_failure();
            }
            Err(e) => {
                error!("Failed to load legacy room {:?}: {}", vk, e);
                worker.mark_fetch_failure();
            }
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
            // A transport Err on the legacy meta send is a connection failure
            // (the slot GETs on the same connection fail too) — mark it (review 9
            // send-side audit); a definitive Ok(None) is a legitimate skip.
            Err(e) => {
                error!("Failed to load legacy rooms_meta: {}", e);
                worker.mark_fetch_failure();
                None
            }
            Ok(_) => None,
        }
    } else {
        None
    };

    if slots.is_empty() {
        // Every listed legacy slot failed to fetch or came back empty. Do NOT
        // write the global load state here (freenet/river#397 Codex review 3): a
        // single legacy delegate finding nothing is NOT authoritative — other
        // concurrent legacy probes may still have rooms, and writing `Loaded` from
        // one empty probe could stomp a peer probe that is mid-`Migrating`. The
        // universal 60s backstop resolves the all-nothing case; a probe that DOES
        // find rooms sets `Migrating` and hydrates (`room_count > 0` → `List`).
        return;
    }

    // is_legacy_delegate = true: skip legacy cursor restore, and trigger the
    // re-save to the CURRENT delegate (which also marks legacy migration done and
    // resolves the load state to `Loaded` on the re-save's success/`Err` arms).
    let loaded = reconstruct_rooms(slots, meta);
    if hydrate_loaded_rooms(
        loaded,
        true,
        source_rank_for_delegate_key(legacy_key.bytes()),
        worker.attempt(),
    ) {
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
fn hydrate_loaded_rooms(
    loaded_rooms: Rooms,
    is_legacy_delegate: bool,
    source_rank: u32,
    attempt: u32,
) -> bool {
    // The usual derivation: a legacy delegate is an older snapshot, the current
    // delegate is authoritative. `migrate_current_blob_to_per_room` is the one
    // caller that needs neither (freenet/river#590) and uses the explicit form.
    let authority = if is_legacy_delegate {
        crate::room_data::MergeAuthority::OlderSnapshot
    } else {
        crate::room_data::MergeAuthority::Authoritative
    };
    hydrate_loaded_rooms_with_authority(
        loaded_rooms,
        is_legacy_delegate,
        source_rank,
        authority,
        attempt,
    )
}

/// [`hydrate_loaded_rooms`] with the merge authority stated explicitly, for the
/// caller whose source is neither the live current delegate nor a legacy one.
fn hydrate_loaded_rooms_with_authority(
    loaded_rooms: Rooms,
    is_legacy_delegate: bool,
    source_rank: u32,
    authority: crate::room_data::MergeAuthority,
    attempt: u32,
) -> bool {
    // freenet/river#397: a legacy-delegate response means we found the user's
    // rooms under an OLD delegate key and are about to migrate them (Ivvor's
    // case). Announce `Migrating` BEFORE the ROOMS merge below (which is
    // deferred) so a re-render lands on the "Migrating…" state ahead of the list
    // filling. The current-delegate path (is_legacy_delegate == false) is not a
    // migration and sets its own resolved state at the call site.
    //
    // Review 11: `attempt` is the caller worker's captured attempt. This runs
    // AFTER awaited fetches (and the re-save below is a SPAWNED closure), so gate
    // every load-state write on still-current so a reconnect/retry that superseded
    // the caller isn't polluted.
    if is_legacy_delegate {
        set_load_state_if_current(attempt, RoomsLoadState::Migrating);
    }

    // Tombstone filter for all downstream loops.
    // Includes both: (a) tombstones in the
    // incoming loaded_rooms, and (b) tombstones
    // already in the current in-memory ROOMS.
    //
    // NOTE (freenet/river#590): this local set is
    // used only to filter `room_keys` for the
    // sync/subscribe loops below — it is NOT what
    // decides removal. The #247 rationale it used
    // to carry ("legacy delegates predate the
    // tombstone field, so the receiver's set is
    // authoritative") has outlived its premise:
    // `legacy_delegates.toml` gains an entry on
    // every WASM bump, so recent legacy
    // generations carry per-room tombstones
    // routinely. Removal is decided in
    // `Rooms::merge_from_source`, where tombstones
    // are RANKED against the copy held.
    //
    // freenet/river#590 follow-up: a room in `ROOMS.removed_rooms` can now come
    // BACK OUT of it — that is the whole point of ranking tombstones. So a room
    // this response will RESTORE must not be filtered out here, or it is rescued
    // from deletion and then arrives inert: never subscribed, never synced, its
    // signing key never migrated to the delegate. The survival rule is asked of
    // `MergeRanks` rather than re-derived, so this filter cannot drift from the
    // merge that runs (deferred) a moment later.
    //
    // This filter is answering ahead of a deferred merge, so an INDIVIDUAL answer
    // can be wrong in either direction when responses interleave — and it is
    // still sufficient, for a reason worth writing down because nobody will
    // reconstruct it. Over-inclusion is benign: a duplicate `mark_needs_sync` is
    // idempotent, and the signing loop reads `ROOMS` at defer time so it migrates
    // whichever identity actually won. Under-inclusion cannot lose a room,
    // because a tombstone rank only ever RISES (`.max()`) or is removed, and
    // removal happens solely in the restore branch — which only a response
    // carrying `Present` for that room can take, and whose own filter therefore
    // included it. So every room that ends up in the map appeared in at least one
    // response's `room_keys`, which is all the per-key, idempotent loops need.
    let incoming_rank = authority.rank_for(source_rank);
    let tombstoned: std::collections::HashSet<ed25519_dalek::VerifyingKey> = {
        let mut t = loaded_rooms.removed_rooms.clone();
        let cur = ROOMS.read();
        for vk in &cur.removed_rooms {
            t.insert(*vk);
        }
        crate::components::app::chat_delegate::with_identity_source_ranks(|ranks| {
            t.retain(|vk| !ranks.presence_survives_tombstone(&vk.to_bytes(), incoming_rank));
        });
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

    // Collect the room keys before the merge, since `loaded_rooms` is moved
    // into the deferred merge below. These are the rooms THIS response brought;
    // the identity each one ends up with is whatever the merge settles on, read
    // back afterwards (freenet/river#527).
    // Filter out tombstoned rooms so we don't
    // re-subscribe / re-sync rooms the user
    // explicitly left (skeptical-review H1).
    let room_keys: Vec<_> = loaded_rooms
        .map
        .keys()
        .copied()
        .filter(|k| !tombstoned.contains(k))
        .collect();
    // Merge the loaded rooms with the current rooms
    let loaded_keys = room_keys.clone();
    crate::util::defer(move || {
        ROOMS.with_mut(|current_rooms| {
            // Ranked by SOURCE, not arrival order (freenet/river#527): the
            // legacy fan-out hydrates from every delegate generation at once,
            // so an identity conflict is decided by which generation the copy
            // came from.
            let merged = crate::components::app::chat_delegate::with_identity_source_ranks(
                |identity_ranks| {
                    current_rooms.merge_from_source(
                        loaded_rooms,
                        source_rank,
                        authority,
                        identity_ranks,
                    )
                },
            );
            // freenet/river#591: report the failure, then run the post-processing
            // ANYWAY. The merge is now per-room isolated, so an `Err` means "one
            // or more rooms failed" and not "nothing merged" — skipping the two
            // loops below would leave every room that DID merge without its
            // decrypted secrets or its rebuilt actions_state, rendering
            // `[Encrypted message - secret vN not available]` until reload.
            if let Err(e) = merged {
                error!("Failed to merge one or more rooms (others still merged): {e}");
            } else {
                info!("Successfully merged rooms from delegate");
            }
            // freenet/river#590: tell the SAVE path about any room the ranked
            // merge just brought back. It cannot infer it — `MergeRanks` is never
            // persisted and `RoomSlot::Tombstone` carries no rank, so
            // `reconcile_room_present` would see a delegate tombstone, answer
            // `AbortAdoptLeave` and write nothing. The room would then be alive in
            // memory for this session and tombstoned on the delegate for good,
            // which is #590's symptom with an extra session in front of it.
            //
            // Drained outside the ranks lock: `mark_room_rejoined` takes it too,
            // and the mutex is not reentrant.
            let resurrected: Vec<[u8; 32]> =
                crate::components::app::chat_delegate::with_identity_source_ranks(|ranks| {
                    ranks.resurrected.drain().collect()
                });
            // NOTE — this widens what `REJOINED_THIS_SESSION` means. It used to
            // say only "the user deliberately did something" (accepted an
            // invitation, imported an identity); it now also says "the ranks
            // concluded this room should come back". That is what makes
            // `present_action(Tombstone, true) = StoreFresh` overwrite the
            // delegate's tombstone — so a WRONG resurrection is no longer
            // session-local, it is persisted. The known wrong-resurrection case
            // is a version downgrade (rank is a proxy for wall-clock, which
            // running an older build after a newer one breaks). Accepted
            // deliberately: a wrongly-resurrected room is one the user leaves
            // again, whereas #590 destroys `self_sk`, which is unrecoverable.
            for room_key in resurrected {
                if let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&room_key) {
                    info!("Merge resurrected a room a newer generation still holds; marking rejoined so the save path adopts it");
                    crate::components::app::chat_delegate::mark_room_rejoined(vk);
                }
            }
            {
                // Re-decrypt ALL secret versions for each room (secrets are #[serde(skip)])
                for room_data in current_rooms.map.values_mut() {
                    let decrypted = room_data.repopulate_secrets_from_state();
                    if room_data.is_private() {
                        info!(
                            "LoadRooms merge: decrypted {} room secret(s) for {:?}",
                            decrypted,
                            room_data.self_member_id()
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

    // Migrate signing keys to delegate for each loaded room.
    //
    // DEFERRED so it runs AFTER the merge above — both go through
    // `crate::util::defer` (setTimeout(0), FIFO), the same ordering
    // `mark_needs_sync` below already relies on — and reads the identity that
    // actually WON that merge rather than the one this particular response
    // happened to carry (freenet/river#527).
    //
    // Reading it BEFORE the merge was the second half of the rollback: when the
    // merge KEPT a different identity for a room, this loop still pushed the
    // incoming (losing) key to the delegate, and `migrate_signing_key`
    // overwrites on mismatch ("Delegate has stale key for room") — so the
    // delegate was left signing with an identity the UI had already rejected,
    // and every signature it produced was invalid for the room.
    let loaded_keys_for_signing = loaded_keys;
    crate::util::defer(move || {
        let signing_keys: Vec<_> = {
            let rooms = ROOMS.peek();
            loaded_keys_for_signing
                .iter()
                .filter_map(|vk| {
                    // Absent => the merge dropped this room (tombstoned, or an
                    // identity conflict it resolved against this copy). Nothing
                    // to migrate; pushing a key for it is exactly the bug above.
                    let room_data = rooms.map.get(vk)?;
                    // No local private key => nothing to migrate INTO the
                    // delegate; the whole point of this pass is to copy the
                    // key we hold. Skip the room rather than push a `None`.
                    let self_sk = room_data.signing_key()?.clone();
                    let owns_room = room_data.is_self_owner();
                    // Derive the contract id from the CURRENT bundled
                    // room-contract WASM so an owner-mode subscription can't
                    // target a contract generation that no longer exists
                    // (Codex P1 on PR #276 round 2).
                    let contract_id_for_owner: Option<[u8; 32]> = if owns_room {
                        Some(**crate::util::owner_vk_to_contract_key(&room_data.owner_vk).id())
                    } else {
                        None
                    };
                    Some((*vk, room_data.room_key(), self_sk, contract_id_for_owner))
                })
                .collect()
        };
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
                    // HYDRATION: startup LoadRooms re-migrating an already-stored
                    // key — NOT a new identity choice, so it must not override the
                    // registry and is discarded if superseded (freenet/river#414 P1).
                    crate::signing::migrate_signing_key(delegate_room_key, &signing_key, false)
                        .await;

                    if result != crate::signing::MigrationResult::Failed {
                        // Must defer signal mutations from spawn_local to
                        // avoid RefCell already borrowed panics in Dioxus runtime
                        let migrated_key = signing_key.clone();
                        crate::util::defer(move || {
                            let mut sanitized = false;
                            ROOMS.with_mut(|rooms| {
                                if let Some(room_data) = rooms.map.get_mut(&room_key_copy) {
                                    // Only mark migrated for the identity that actually
                                    // completed: an overwrite may have replaced `self_sk`
                                    // while this migration ran, and marking the room
                                    // migrated for a superseded key would strand the new
                                    // identity (freenet/river#414).
                                    // Identity comparison, not raw-key: a copy
                                    // with no local key is not the identity
                                    // that completed this migration either, so
                                    // it must not be marked migrated.
                                    if room_data.self_verifying_key()
                                        != Some(migrated_key.verifying_key())
                                    {
                                        return;
                                    }
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
    });

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
        // Mark the migration in progress BEFORE the re-save. The save writes one
        // per-room key at a time, so an interrupted migration (a per-room CAS
        // failure, or the tab closing mid-save) can leave a PARTIAL per-room set.
        // If we don't record that, the next load takes the per-room path and
        // never re-probes legacy, stranding any room whose key wasn't written
        // until the user rejoins (freenet/river#345 follow-up). The flag stays
        // set until a FULL re-save succeeds, so the next load knows to re-fill.
        mark_legacy_migration_in_progress();
        // freenet/river#397 Codex review 4: only resolve to `Loaded` when this
        // legacy re-save actually merged live rooms. A zero-live-room / empty
        // legacy result writes NOTHING to the global state — so a concurrent
        // legacy probe that DID find rooms (and set `Migrating`) can't be stomped
        // into a false Empty by this one. The universal backstop owns the
        // all-nothing terminal (→ `LoadFailed` if a fetch failed, else Empty).
        crate::util::safe_spawn_local(async move {
            match save_rooms_to_delegate().await {
                Ok(_) => {
                    info!("Successfully migrated room data to new delegate");
                    clear_legacy_migration_in_progress();
                    // Seal only once the whole legacy fan-out has gone quiet —
                    // NOT here (freenet/river#527). Every generation is probed
                    // at once; sealing on the FIRST one to finish its re-save
                    // ends the migration for good while newer generations may
                    // still be answering. If the tab closed in that window, the
                    // current delegate was left holding the older copy, it is
                    // now non-empty, and no later session ever re-probes — which
                    // is what turned a transient identity rollback into a
                    // permanent one.
                    request_legacy_seal_on_quiescence();
                    // Review 11: this runs after the save await, so gate on the
                    // captured attempt (a reconnect/retry may have superseded us).
                    if had_loaded_rooms {
                        set_load_state_if_current(attempt, RoomsLoadState::Loaded);
                    }
                }
                Err(e) => {
                    error!("Failed to migrate room data to new delegate: {}", e);
                    // Leave the in-progress flag set so the next load re-fills.
                    if had_loaded_rooms {
                        set_load_state_if_current(attempt, RoomsLoadState::Loaded);
                    }
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

/// Short, cheap fingerprint of a `DelegateKey` for logs.
///
/// `DelegateKey`'s `Debug` prints the 32-byte key as a decimal byte array, which
/// is ~450 bytes of console text per line and is emitted on every delegate
/// response. Nobody triages from the raw bytes; a stable prefix is enough to
/// tell one delegate from another. freenet/river#569.
pub(crate) fn delegate_key_fp(key: &freenet_stdlib::prelude::DelegateKey) -> String {
    // Hex, not base58: `legacy_delegates.toml` records `delegate_key` in hex, so a
    // fingerprint from a log greps directly against that registry. (`DelegateKey`'s
    // own Display is base58 and would not.) No length suffix: `bytes()` is always
    // the 32-byte hash.
    let head: String = key
        .bytes()
        .iter()
        .take(4)
        .map(|x| format!("{x:02x}"))
        .collect();
    format!("{head}..")
}

/// A delegate storage key rendered for humans.
///
/// These keys are byte strings that are almost always UTF-8 names
/// (`outbound_dms`, `rooms_meta`, `room:<base58 vk>`), so printing the text is
/// both far cheaper than the byte array AND more useful for triage. Falls back
/// to a length for genuinely binary keys.
fn delegate_storage_key_str(key: &ChatDelegateKey) -> String {
    /// Longest key text rendered. Every key today is a fixed constant or
    /// `"room:" + bs58(vk)` (~49 chars), so this never truncates in practice; it
    /// is here so the worst case is bounded by construction rather than by
    /// assumption about who builds keys later.
    const MAX: usize = 128;
    match std::str::from_utf8(key.as_bytes()) {
        // `is_control` also blocks \n and \r, so a key cannot inject a log line.
        Ok(s) if s.chars().all(|c| !c.is_control()) => {
            if s.len() <= MAX {
                s.to_string()
            } else {
                format!("{}...({} chars)", &s[..MAX], s.len())
            }
        }
        _ => format!("<{} bytes>", key.as_bytes().len()),
    }
}

/// One-line summary of a delegate response, replacing its full `Debug`.
///
/// The `Debug` of a `ListResponse` prints every key as a decimal byte array. On a
/// real account that measured **~401 KB per line across 39 lines, 98.5% of all
/// console bytes River emits during startup** (freenet/river#569), and the
/// per-key bytes were never what anyone read. This keeps the variant, the key
/// names and the sizes -- everything the logs were actually used for.
fn describe_delegate_response(r: &ChatDelegateResponseMsg) -> String {
    use ChatDelegateResponseMsg as R;
    match r {
        R::GetResponse { key, value } => format!(
            "GetResponse {{ key: {}, value: {} }}",
            delegate_storage_key_str(key),
            value
                .as_ref()
                .map_or("none".into(), |v| format!("{} bytes", v.len()))
        ),
        R::ListResponse { keys } => {
            // Name every key but never their bytes: the key set is the point of a
            // ListResponse, and there are tens of them, not thousands.
            let names: Vec<String> = keys.iter().map(delegate_storage_key_str).collect();
            format!(
                "ListResponse {{ {} keys: [{}] }}",
                names.len(),
                names.join(", ")
            )
        }
        R::StoreResponse {
            key,
            value_size,
            result,
        } => format!(
            "StoreResponse {{ key: {}, value_size: {}, ok: {} }}",
            delegate_storage_key_str(key),
            value_size,
            result.is_ok()
        ),
        R::DeleteResponse { key, result } => format!(
            "DeleteResponse {{ key: {}, ok: {} }}",
            delegate_storage_key_str(key),
            result.is_ok()
        ),
        R::GetVersionedResponse {
            key,
            value,
            generation,
        } => format!(
            "GetVersionedResponse {{ key: {}, value: {}, generation: {} }}",
            delegate_storage_key_str(key),
            value
                .as_ref()
                .map_or("none".into(), |v| format!("{} bytes", v.len())),
            generation
        ),
        R::CasStoreResponse { key, result } => {
            // `CasStoreResult::Conflict` carries `current_value: Option<Vec<u8>>` --
            // the FULL stored blob. Debug-dumping it is a BIGGER line than the one
            // this whole change removes (a per-room blob is ~343 KB in memory, so
            // ~4x that as a decimal array), and `cas_write_delegate_key` retries, so
            // one contended save emits several. The adjacent processing code already
            // destructures this correctly; match it. freenet/river#569.
            let r = match result {
                CasStoreResult::Stored { generation } => format!("Stored {{ gen: {generation} }}"),
                CasStoreResult::Conflict {
                    current_generation,
                    current_value,
                } => format!(
                    "Conflict {{ gen: {}, current: {} }}",
                    current_generation,
                    current_value
                        .as_ref()
                        .map_or("none".to_string(), |v| format!("{} bytes", v.len()))
                ),
                CasStoreResult::Failed(e) => format!("Failed({e})"),
            };
            format!(
                "CasStoreResponse {{ key: {}, result: {} }}",
                delegate_storage_key_str(key),
                r
            )
        }
        R::StoreSigningKeyResponse { room_key, result } => format!(
            "StoreSigningKeyResponse {{ room: {}, ok: {} }}",
            room_key_fp(room_key),
            result.is_ok()
        ),
        R::GetPublicKeyResponse {
            room_key,
            public_key,
        } => format!(
            "GetPublicKeyResponse {{ room: {}, public_key: {} }}",
            room_key_fp(room_key),
            if public_key.is_some() {
                "present"
            } else {
                "absent"
            }
        ),
        R::SignResponse {
            room_key,
            request_id,
            signature,
        } => format!(
            "SignResponse {{ room: {}, request_id: {:?}, ok: {} }}",
            room_key_fp(room_key),
            request_id,
            signature.is_ok()
        ),
        R::EnsureRoomSubscriptionResponse {
            room_owner_vk,
            request_id,
            result,
        } => format!(
            "EnsureRoomSubscriptionResponse {{ room: {}, request_id: {:?}, ok: {} }}",
            room_key_fp(room_owner_vk),
            request_id,
            result.is_ok()
        ),
        // Deliberately NO catch-all arm. An exhaustive match makes a newly added
        // variant a COMPILE error here, which is the only thing that reliably stops
        // the next blob-carrying variant from being Debug-dumped -- a source pin
        // cannot see a variant that does not exist yet. It also avoids
        // `format!("{other:?}")`, which builds the entire Debug string in WASM
        // before discarding all but the variant name.
    }
}

/// Short fingerprint of a room key (a 32-byte verifying key) for logs.
fn room_key_fp(room_key: &river_core::chat_delegate::RoomKey) -> String {
    let head: String = room_key
        .iter()
        .take(4)
        .map(|x| format!("{x:02x}"))
        .collect();
    format!("{head}..")
}

#[cfg(test)]
mod tests {
    // --- startup console volume (freenet/river#569) ---

    /// A `ListResponse` summary must name its keys and stay small. Its `Debug`
    /// printed every key as a decimal byte array, measured at ~401 KB per line
    /// across 39 lines during startup: 98.5% of all console bytes River emits.
    #[test]
    fn list_response_summary_names_keys_without_dumping_bytes() {
        use river_core::chat_delegate::{ChatDelegateKey, ChatDelegateResponseMsg};
        let keys: Vec<ChatDelegateKey> = ["outbound_dms", "rooms_meta", "room:AbCdEf"]
            .iter()
            .map(|k| ChatDelegateKey::new(k.as_bytes().to_vec()))
            .collect();
        let out = describe_delegate_response(&ChatDelegateResponseMsg::ListResponse { keys });

        assert!(
            out.contains("outbound_dms"),
            "key names are the useful part: {out}"
        );
        assert!(out.contains("rooms_meta"), "{out}");
        assert!(out.contains("3 keys"), "count should be present: {out}");
        // The failure mode is a decimal byte array. "111, 117, 116" is
        // "out" as bytes -- if that appears, the Debug dump is back.
        assert!(
            !out.contains("111, 117, 116"),
            "must not print key bytes: {out}"
        );
        assert!(
            out.len() < 200,
            "summary should stay small, got {}: {out}",
            out.len()
        );
    }

    /// The same summary on a realistically large key set must stay bounded in the
    /// per-key cost, since that is what made the original line 401 KB.
    #[test]
    fn list_response_summary_scales_with_names_not_bytes() {
        use river_core::chat_delegate::{ChatDelegateKey, ChatDelegateResponseMsg};
        // 40 per-room keys, the shape a real account produces.
        let keys: Vec<ChatDelegateKey> = (0..40)
            .map(|i| ChatDelegateKey::new(format!("room:key{i:034}").into_bytes()))
            .collect();
        let msg = ChatDelegateResponseMsg::ListResponse { keys };
        let out = describe_delegate_response(&msg);
        // Compare against the Debug form directly, not an absolute bound: an
        // absolute bound also passes for a helper that drops the names entirely,
        // and the claim being tested is relative. Each key is 42 chars of text vs
        // ~192 chars as a decimal byte array.
        let debug_len = format!("{msg:?}").len();
        assert!(
            out.len() < debug_len / 3,
            "summary ({} chars) should be far smaller than Debug ({} chars)",
            out.len(),
            debug_len
        );
        assert!(
            out.contains("room:key"),
            "names must still be present: {out}"
        );
    }

    /// Values are reported as sizes, never contents.
    #[test]
    fn get_response_summary_reports_size_not_contents() {
        use river_core::chat_delegate::{ChatDelegateKey, ChatDelegateResponseMsg};
        let out = describe_delegate_response(&ChatDelegateResponseMsg::GetResponse {
            key: ChatDelegateKey::new(b"rooms_meta".to_vec()),
            value: Some(vec![7u8; 4096]),
        });
        assert!(out.contains("rooms_meta"), "{out}");
        assert!(out.contains("4096 bytes"), "size is the useful part: {out}");
        assert!(!out.contains("7, 7, 7"), "must not print the value: {out}");
    }

    /// `CasStoreResult::Conflict` carries the FULL stored blob. Debug-dumping it
    /// is a bigger line than the one this change removes, and a contended save
    /// retries several times. Found in review of PR #570, after the first version
    /// of this fix reintroduced the defect it was removing.
    #[test]
    fn cas_conflict_summary_reports_blob_size_not_contents() {
        use river_core::chat_delegate::{CasStoreResult, ChatDelegateKey, ChatDelegateResponseMsg};
        let out = describe_delegate_response(&ChatDelegateResponseMsg::CasStoreResponse {
            key: ChatDelegateKey::new(b"room:AbCdEf".to_vec()),
            result: CasStoreResult::Conflict {
                current_generation: 7,
                current_value: Some(vec![9u8; 8192]),
            },
        });
        assert!(out.contains("room:AbCdEf"), "{out}");
        assert!(
            out.contains("gen: 7"),
            "generation is the actionable part: {out}"
        );
        assert!(out.contains("8192 bytes"), "blob reported as a size: {out}");
        assert!(!out.contains("9, 9, 9"), "must not print the blob: {out}");
        assert!(out.len() < 120, "stayed small: {out}");
    }

    /// A binary (non-UTF-8) key must not be printed as bytes either.
    #[test]
    fn binary_storage_key_falls_back_to_a_length() {
        use river_core::chat_delegate::ChatDelegateKey;
        let out = delegate_storage_key_str(&ChatDelegateKey::new(vec![0xff, 0xfe, 0x00]));
        assert_eq!(out, "<3 bytes>", "got {out}");
    }

    /// Regression pin: the delegate-response log sites must not Debug-format the
    /// whole decoded response or the raw payload. Both were reintroduced easily
    /// (they are the obvious thing to write) and together they were essentially
    /// all of River's startup console output.
    #[test]
    fn delegate_response_logs_do_not_debug_dump_payloads() {
        let src = include_str!("response_handler.rs");
        // `mod tests {` is this file's idiom for the cut (20 other pins use it);
        // `#[cfg(test)]` also decorates non-test items and can truncate early.
        let prod = src.split("mod tests {").next().unwrap_or(src);
        assert!(
            !prod.contains("as ChatDelegateResponseMsg: {:?}"),
            "the decoded response must be summarised via describe_delegate_response, \
             not Debug-dumped: a ListResponse Debug is ~401 KB per line (river#569)"
        );
        assert!(
            prod.contains("describe_delegate_response(&response)"),
            "the summary helper must be wired into the log site"
        );
        assert!(
            !prod.contains("payload_str"),
            "the payload must be logged as a size, not a formatted byte array \
             (river#569). Pinned on the variable rather than the format string so \
             a reformat or a longer expression cannot evade it."
        );
        assert!(
            prod.contains("ApplicationMessage payload: {} bytes"),
            "payload log should report a size"
        );
        // The failure branch keeps a short hex head: without it a wire-format
        // break is indistinguishable from a corrupt frame in a user's console.
        assert!(
            prod.contains("Failed to deserialize chat delegate response \\\n"),
            "the deser-failure log should carry a hex head"
        );
    }

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
    /// Regression pin (code-first/skeptical re-review): a legacy delegate
    /// returning NO `rooms_data` blob must NOT mark legacy migration done — its
    /// dynamic `room:<vk>` keys may still need migrating via the List-driven
    /// `migrate_legacy_per_room`, and an early mark-done would permanently strand
    /// them on any transient migration failure (the V24→V25 data-loss). Pin: the
    /// empty-`rooms_data` `is_legacy_delegate` branch (between the two stable
    /// log markers) must contain no `mark_legacy_migration_done(` call.
    #[test]
    fn legacy_empty_rooms_data_does_not_mark_migration_done() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");
        let start = production
            .find("No rooms data found in delegate")
            .expect("empty-rooms_data branch marker must exist");
        let end = production[start..]
            .find("Current delegate empty — firing legacy migration")
            .expect("current-empty branch marker must exist")
            + start;
        assert!(
            !production[start..end].contains("mark_legacy_migration_done("),
            "the empty-rooms_data legacy branch must NOT mark migration done — \
             per-room keys may still need migrating (code-first/skeptical re-review)"
        );
    }

    /// Regression pin (freenet/river#345 follow-up — Nacho's "Freenet Devs"
    /// disappeared-after-update): an interrupted migration must be recoverable.
    /// (1) The legacy re-save marks migration IN PROGRESS before saving and only
    /// clears it on success, so a partial/aborted migration is detectable.
    /// (2) The per-room load, when a migration was left in progress, must NOT
    /// blindly mark done and must re-run the migration to recover stranded rooms.
    #[test]
    fn interrupted_migration_is_recovered_on_next_load() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");

        // (1) Legacy re-save: in-progress set before save, cleared on success.
        let resave_start = production
            .find("Migrating room data from legacy delegate to new delegate")
            .expect("legacy re-save block marker must exist");
        let resave = &production[resave_start..(resave_start + 1800).min(production.len())];
        assert!(
            resave.contains("mark_legacy_migration_in_progress()"),
            "legacy re-save must mark migration in progress BEFORE saving"
        );
        assert!(
            resave.contains("clear_legacy_migration_in_progress()"),
            "legacy re-save must clear the in-progress flag on success"
        );

        // (2) Per-room load routes its gating through the pure, unit-tested
        //     decision helper (see `decide_per_room_load_action` tests in
        //     chat_delegate.rs for the truth table), and the recovery call is
        //     scoped INSIDE the `action.recover` branch. The scoping matters:
        //     `migrate_current_blob_to_per_room().await;` also appears in the
        //     unrelated `LoadPlan::MigrateCurrentBlob` arm, so a file-wide
        //     `contains` would pass even if the recovery call were deleted.
        assert!(
            production.contains("let migration_interrupted = is_legacy_migration_in_progress();")
                && production.contains("decide_per_room_load_action(migration_interrupted)"),
            "per-room load must derive its action from the in-progress flag via the helper"
        );
        let recover_idx = production
            .find("if action.recover")
            .expect("per-room load must have an `if action.recover` recovery branch");
        let recover_block = &production[recover_idx..(recover_idx + 700).min(production.len())];
        assert!(
            recover_block.contains("migrate_current_blob_to_per_room(true).await;"),
            "the recovery branch must re-run the migration (as a background re-fill, \
             recovery = true) to recover stranded rooms — see freenet/river#397 review"
        );
        assert!(
            recover_block.contains("arm_legacy_migration_recovery()"),
            "recovery must be armed once-per-session (clears the attempt guard so \
             same-session recovery isn't a no-op — codex review of #352)"
        );
    }

    /// freenet/river#397 review: both migration re-save `Err` arms must resolve
    /// the load to `Loaded` when the migration merged live rooms (`had_rooms`), so
    /// a save error can't strand the rail on "Migrating…". A zero-live-room result
    /// intentionally writes NOTHING (the universal backstop owns that terminal —
    /// Codex review 4 concurrency fix), so the pinned write is `had_rooms`-gated,
    /// not unconditional. This pins that neither arm silently drops the resolution.
    #[test]
    fn migration_save_error_resolves_load_state() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");

        // (a) current-delegate blob explosion save error.
        let blob_err = production
            .find("Failed to explode single blob into per-room keys")
            .expect("blob-explosion save-error marker must exist");
        let blob_arm = &production[blob_err..(blob_err + 900).min(production.len())];
        assert!(
            blob_arm.contains("set_load_state_if_current(attempt, RoomsLoadState::Loaded)"),
            "the blob-explosion save-error arm must resolve to Loaded (had_rooms), attempt-gated (review 11)"
        );

        // (b) legacy-delegate re-save error.
        let legacy_err = production
            .find("Failed to migrate room data to new delegate")
            .expect("legacy re-save-error marker must exist");
        let legacy_arm = &production[legacy_err..(legacy_err + 900).min(production.len())];
        assert!(
            legacy_arm.contains("set_load_state_if_current(attempt, RoomsLoadState::Loaded)"),
            "the legacy re-save-error arm must resolve to Loaded (had_rooms), attempt-gated (review 11)"
        );
    }

    /// freenet/river#397 Codex review 5: every failure to load KNOWN or STORED
    /// room data routes to `mark_fetch_failure()` + a LoadFailed-capable terminal,
    /// never a silent Empty. Pins the three stored-data failure sites this pass
    /// added/changed.
    #[test]
    fn stored_room_data_failures_route_to_loadfailed() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");

        // (P2b) corrupt current-delegate `rooms_data` blob → mark_fetch_failure +
        // LoadFailed (a blob exists only because rooms were written to it), both
        // attempt-scoped through the worker (review 7).
        let corrupt = production
            .find("Failed to deserialize current-delegate rooms blob")
            .expect("corrupt-current-blob marker must exist");
        let corrupt_arm = &production[corrupt..(corrupt + 700).min(production.len())];
        assert!(
            corrupt_arm.contains("worker.mark_fetch_failure()")
                && corrupt_arm.contains("worker.set_load_state(RoomsLoadState::LoadFailed)"),
            "a corrupt current rooms_data blob must worker.mark_fetch_failure + LoadFailed, not Empty"
        );

        // (audit) SEND failure reading the current blob (we're here only because
        // the index listed a rooms_data key) → mark_fetch_failure.
        let read_err = production
            .find("Failed to read current-delegate rooms blob")
            .expect("current-blob read-error marker must exist");
        let read_arm = &production[read_err..(read_err + 400).min(production.len())];
        assert!(
            read_arm.contains("mark_fetch_failure()"),
            "a failed READ of the current rooms_data blob must mark_fetch_failure"
        );

        // (audit) parse failure of a legacy `rooms_data` blob → mark_fetch_failure
        // (a genuine new user never reaches this — no legacy delegate responds).
        let legacy_parse = production
            .find("Failed to deserialize rooms data")
            .expect("legacy rooms_data parse-error marker must exist");
        let legacy_parse_arm =
            &production[legacy_parse..(legacy_parse + 1500).min(production.len())];
        assert!(
            legacy_parse_arm.contains("worker.mark_fetch_failure()"),
            "a legacy rooms_data parse failure must worker.mark_fetch_failure (settlement → LoadFailed)"
        );
    }

    /// freenet/river#397 review 7 (P2#1): every global write a load worker makes
    /// must be attempt-scoped so a stale worker (superseded by a Retry) can't
    /// corrupt the new attempt. Pin that NO bare `mark_fetch_failure()` survives
    /// (all go through `worker.mark_fetch_failure()`) and the worker-body terminal
    /// writes go through `worker.set_load_state(...)`.
    #[test]
    fn worker_global_writes_are_attempt_scoped() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");

        // Every `mark_fetch_failure()` occurrence is part of `worker.mark_fetch_failure()`.
        assert_eq!(
            production.matches("mark_fetch_failure()").count(),
            production.matches("worker.mark_fetch_failure()").count(),
            "a bare mark_fetch_failure() in a worker body would pollute a stale attempt's SAW_FETCH_FAILURE"
        );
        assert!(
            production.matches("worker.mark_fetch_failure()").count() >= 5,
            "the current + legacy fetch-failure arms must route through the worker"
        );

        // The worker-body terminal / probe / migrating writes are attempt-scoped.
        for terminal in [
            "worker.set_load_state(per_room_terminal(",
            "worker.set_load_state(load_state_after_probe_legacy(fired))",
            "worker.set_load_state(RoomsLoadState::Migrating)",
            "worker.set_load_state(RoomsLoadState::LoadFailed)",
        ] {
            assert!(
                production.contains(terminal),
                "worker-body write `{terminal}` must be attempt-scoped via worker.set_load_state"
            );
        }
    }

    /// freenet/river#397 Codex review 11 — the class is closed BY CONSTRUCTION:
    /// the response handler has ZERO bare `set_rooms_load_state(` and ZERO bare
    /// `mark_fetch_failure()` — EVERY load-state / failure write (including the
    /// SPAWNED save closures and `hydrate_loaded_rooms`' legacy writes that outlive
    /// their worker's sync body) routes through the attempt-gated
    /// `worker.set_load_state` / `worker.mark_fetch_failure` / `set_load_state_if_current`
    /// helpers. A future edit that adds an ungated async write fails this pin.
    #[test]
    fn no_ungated_async_load_state_writes() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");

        assert!(
            !production.contains("set_rooms_load_state("),
            "no bare set_rooms_load_state — use worker.set_load_state / set_load_state_if_current (attempt-gated)"
        );
        assert_eq!(
            production.matches("mark_fetch_failure()").count(),
            production.matches("worker.mark_fetch_failure()").count(),
            "no bare mark_fetch_failure — all attempt-scoped via worker.mark_fetch_failure"
        );
        // hydrate threads the attempt through, and its legacy writes are gated.
        // Whitespace-stripped: the signature is long enough that rustfmt splits
        // it across lines, and a contiguous needle would go vacuously false the
        // next time a parameter is added.
        let strip_ws = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
        let squeezed = strip_ws(production);
        // Matched piecewise, WITHOUT the closing paren: rustfmt drops the
        // trailing comma if the signature ever fits on one line, which would
        // fail a whole-signature needle spuriously.
        let has_param = |p: &str| squeezed.contains(&strip_ws(p));
        assert!(
            has_param("fn hydrate_loaded_rooms(")
                && has_param("loaded_rooms: Rooms,")
                && has_param("is_legacy_delegate: bool,")
                && has_param("source_rank: u32,")
                && has_param("attempt: u32"),
            "hydrate_loaded_rooms must take an attempt to gate its legacy load-state writes, \
             and a source_rank so identity conflicts are resolved by delegate generation \
             rather than response arrival order (freenet/river#527)"
        );
        assert!(
            production.contains("set_load_state_if_current(attempt, RoomsLoadState::Migrating)"),
            "hydrate's legacy Migrating write must be attempt-gated via set_load_state_if_current"
        );
        // The spawned blob-explosion save closure captures the worker's attempt.
        assert!(
            production.contains("let attempt = worker.attempt();"),
            "the spawned save closure must capture worker.attempt() to gate its late Loaded write"
        );
    }

    /// freenet/river#397 Codex review 9 (send-side audit): a `rooms_meta` GET whose
    /// SEND returns Err is a transport failure (the same connection the per-room
    /// slot GETs use), so it routes to `worker.mark_fetch_failure()` — both the
    /// current-delegate and legacy meta arms — rather than a silent skip.
    #[test]
    fn meta_get_transport_failure_marks_fetch_failure() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");

        // Current-delegate meta arm.
        let cur = production
            .find("Failed to load rooms_meta")
            .expect("current rooms_meta error marker must exist");
        let cur_arm = &production[cur..(cur + 400).min(production.len())];
        assert!(
            cur_arm.contains("worker.mark_fetch_failure()"),
            "a current rooms_meta send-Err must mark_fetch_failure (transport failure)"
        );

        // Legacy meta arm.
        let leg = production
            .find("Failed to load legacy rooms_meta")
            .expect("legacy rooms_meta error marker must exist");
        let leg_arm = &production[leg..(leg + 400).min(production.len())];
        assert!(
            leg_arm.contains("worker.mark_fetch_failure()"),
            "a legacy rooms_meta send-Err must mark_fetch_failure (transport failure)"
        );
    }

    /// freenet/river#397 Codex review 3: in `migrate_current_blob_to_per_room`,
    /// suppress-under-recovery applies ONLY to a DOWNGRADE (the value-gone
    /// `load_state_after_probe_legacy` write, which could push a resolved `Loaded`
    /// back to `Loading`). A save COMPLETION (Ok or Err → `Loaded`) must resolve
    /// EVEN under recovery — otherwise a recovery whose save completes leaves the
    /// rail stuck. Pin: (a) the signature threads `recovery`; (b) the value-gone
    /// downgrade write is `!recovery`-guarded; (c) the save-completion closure
    /// resolves `Loaded` WITHOUT a `!recovery` guard.
    #[test]
    fn recovery_re_run_never_downgrades_load_state_to_loading() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");

        assert!(
            production.contains("async fn migrate_current_blob_to_per_room(recovery: bool)"),
            "migrate_current_blob_to_per_room must thread a `recovery` flag so a \
             background re-fill can suppress only its DOWNGRADE write"
        );
        // The initial (state-owning) caller passes false; the recovery caller
        // passes true (the latter is also pinned by
        // `interrupted_migration_is_recovered_on_next_load`).
        assert!(
            production.contains("migrate_current_blob_to_per_room(false).await;"),
            "the initial LoadPlan::MigrateCurrentBlob caller must own the load state (recovery = false)"
        );
        // Slice just the `migrate_current_blob_to_per_room` body (from its
        // signature to the next top-level `async fn`) so the checks below can't
        // accidentally match the identically-named write in `load_rooms_per_room`'s
        // `ProbeLegacy` arm (which is correctly ungated — it OWNS the load).
        let fn_start = production
            .find("async fn migrate_current_blob_to_per_room(recovery: bool)")
            .expect("migrate_current_blob_to_per_room signature must exist");
        let body_after_sig = &production[fn_start + 1..];
        let fn_end = body_after_sig
            .find("\nasync fn ")
            .map(|i| fn_start + 1 + i)
            .unwrap_or(production.len());
        let body = &production[fn_start..fn_end];

        // (b) the value-gone probe-legacy write — the ONLY downgrade — is
        // `!recovery`-guarded, directly enclosed by the guard (same block). It is
        // attempt-scoped via `worker.set_load_state` (review 7).
        let write_pos = body
            .find("worker.set_load_state(load_state_after_probe_legacy(fired))")
            .expect("the value-gone branch must write via load_state_after_probe_legacy");
        let guard_pos = body[..write_pos]
            .rfind("if !recovery {")
            .expect("the value-gone downgrade write must be guarded on !recovery");
        assert!(
            write_pos - guard_pos < 120,
            "the value-gone probe-legacy write must be DIRECTLY inside the `if !recovery` \
             block so a background recovery never downgrades a resolved Loaded to Loading"
        );

        // (c) the save-completion closure resolves `Loaded` WITHOUT a `!recovery`
        // guard — a completion must resolve even under recovery (Codex review 3).
        let save_pos = body
            .find("match save_rooms_to_delegate().await {")
            .expect("the blob-explosion save closure must exist");
        // Bound to the closure body (up to the corrupt-blob `Err(e) =>` arm that
        // follows the spawn).
        let save_end = body[save_pos..]
            .find("A corrupt current-delegate blob")
            .map(|i| save_pos + i)
            .unwrap_or(body.len());
        let save_closure = &body[save_pos..save_end];
        assert!(
            save_closure.contains("set_load_state_if_current(attempt, RoomsLoadState::Loaded)"),
            "the save-completion arms must resolve to Loaded (attempt-gated, review 11)"
        );
        assert!(
            !save_closure.contains("if !recovery"),
            "a save COMPLETION must resolve Loaded even under recovery — only the \
             value-gone DOWNGRADE is !recovery-guarded (Codex review 3)"
        );
    }

    /// freenet/river#397 Codex review 3 (P1): `migrate_legacy_per_room` fetches
    /// each legacy slot sequentially and hydrates (which sets `Migrating`) only at
    /// the end. Without an early `Migrating`, the whole fetch window sits at
    /// `Loading`, so the backstop could flip it to Empty despite the legacy index
    /// proving rooms exist. Pin that it sets `Migrating` BEFORE the fetch loop.
    /// (It must NOT write `Loaded` on the all-empty early return — a single legacy
    /// probe is not authoritative; the universal backstop resolves that case.)
    #[test]
    fn migrate_legacy_per_room_shows_migrating_before_fetch_loop() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");

        let fn_start = production
            .find("async fn migrate_legacy_per_room(")
            .expect("migrate_legacy_per_room must exist");
        // Bound at the NEAREST next top-level item — `migrate_legacy_per_room` is
        // followed by a plain `fn` (`hydrate_loaded_rooms`), so bounding only on
        // `async fn` would over-run and pick up hydrate's load-state writes.
        let rest = &production[fn_start + 1..];
        let fn_end = [rest.find("\nasync fn "), rest.find("\nfn ")]
            .into_iter()
            .flatten()
            .min()
            .map(|i| fn_start + 1 + i)
            .unwrap_or(production.len());
        let body = &production[fn_start..fn_end];

        // `Migrating` is set (attempt-scoped, review 7) before the fetch loop.
        let migrating_pos = body
            .find("worker.set_load_state(RoomsLoadState::Migrating)")
            .expect("migrate_legacy_per_room must announce Migrating for a non-empty index");
        let loop_pos = body
            .find("for vk in room_vks")
            .expect("migrate_legacy_per_room must have a per-slot fetch loop");
        assert!(
            migrating_pos < loop_pos,
            "Migrating must be set BEFORE the fetch loop so the backstop can't flip \
             a known-non-empty legacy load to Empty mid-fetch"
        );

        // A single legacy probe finding nothing is NOT authoritative — it must NOT
        // write the global load state (concurrent probes may hold rooms). The only
        // load-state write in this function is the `Migrating` above.
        assert_eq!(
            body.matches("worker.set_load_state(").count(),
            1,
            "migrate_legacy_per_room must write the load state exactly once (Migrating); \
             the all-empty case must NOT write Loaded (Codex review 3 — not authoritative)"
        );

        // freenet/river#397 Codex review 4/9/10: a failed legacy fetch records the
        // global SAW_FETCH_FAILURE — the slot transport-Err, the unparseable-slot
        // Err, the unexpected-variant `Ok(other)` arm (review 10 P2#2), AND the
        // meta-GET transport Err (review 9) — so the backstop resolves to
        // LoadFailed rather than a false Empty; NOT a `value: None` skip.
        assert_eq!(
            body.matches("mark_fetch_failure()").count(),
            4,
            "legacy slot transport-Err, unparseable-slot Err, Ok(other), and meta-GET Err arms must mark"
        );
        // The `value: None` arm (definitive missing value) is a legitimate skip,
        // NOT a fetch failure. Bound the window to that arm alone (the adjacent
        // `Ok(other)` arm DOES mark).
        let value_none_pos = body
            .find("listed but value missing")
            .expect("legacy value: None arm must exist");
        let after_none = &body[value_none_pos..(value_none_pos + 55).min(body.len())];
        assert!(
            !after_none.contains("mark_fetch_failure()"),
            "a legacy `value: None` is a legitimate skip, not a fetch failure"
        );
    }

    /// freenet/river#397 Codex review 10 (P2#2): the legacy per-slot GET match
    /// must split the catch-all — an unexpected response variant (`Ok(other)`,
    /// e.g. a delegate protocol mismatch) is a fetch failure (marks), mirroring
    /// the current-delegate path; only a definitive `value: None` is a clean skip.
    #[test]
    fn legacy_per_slot_unexpected_variant_marks_fetch_failure() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");
        // Slice the legacy per-slot loop (marker → the `let meta` that follows it).
        let loop_start = production
            .find("Migrating {} per-room slot(s) from legacy delegate")
            .expect("legacy fetch loop marker must exist");
        let loop_end = production[loop_start..]
            .find("let meta = if has_meta")
            .map(|i| loop_start + i)
            .expect("the legacy loop must be followed by the meta load");
        let loop_region = &production[loop_start..loop_end];
        // The unexpected-variant arm exists and marks.
        let other_pos = loop_region
            .find("Unexpected response loading legacy room")
            .expect("legacy per-slot Ok(other) arm must exist");
        let other_arm = &loop_region[other_pos..(other_pos + 150).min(loop_region.len())];
        assert!(
            other_arm.contains("worker.mark_fetch_failure()"),
            "a legacy per-slot Ok(other) (unexpected variant) must mark_fetch_failure"
        );
    }

    /// freenet/river#397 Codex review 6: EVERY awaited load worker must hold a
    /// `LoadWorkerGuard` so progress-tracked termination sees it start and finish.
    /// Pin that all four worker sites are wrapped: the current-delegate load, the
    /// blob migration, each legacy per-room probe, and the legacy single-blob
    /// hydrate. If a future edit adds a load path without a guard, the counter
    /// under-counts and the load could resolve prematurely — this fails CI.
    #[test]
    fn all_awaited_load_workers_hold_a_guard() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");

        // The four production worker sites each construct a guard.
        assert_eq!(
            production.matches("LoadWorkerGuard::new()").count(),
            4,
            "exactly four awaited load-worker sites must each hold a LoadWorkerGuard"
        );

        // Each of the three async worker fns opens with a guard before doing work.
        for func in [
            "async fn load_rooms_per_room(",
            "async fn migrate_current_blob_to_per_room(",
            "async fn migrate_legacy_per_room(",
        ] {
            let fn_start = production
                .find(func)
                .unwrap_or_else(|| panic!("{func} must exist"));
            // The guard must appear near the top of the body (before the first
            // awaited request), not somewhere deep after work started. Window is
            // generous to allow for the leading doc/rationale comment.
            let head = &production[fn_start..(fn_start + 1100).min(production.len())];
            assert!(
                head.contains("LoadWorkerGuard::new()"),
                "{func} must hold a LoadWorkerGuard from the top"
            );
        }

        // The legacy single-blob GetResponse parse block is the 4th site: the
        // guard wraps BOTH parse arms (so the Err arm is attempt-scoped and its
        // settlement resolves LoadFailed — review 7 P2#2). Confirm a guard is
        // constructed immediately before the `from_reader::<Rooms>(&rooms_data..)`
        // match that both arms belong to.
        let parse = production
            .find("from_reader::<Rooms, _>(&rooms_data[..])")
            .expect("legacy GetResponse rooms_data parse must exist");
        let guard_before = production[..parse]
            .rfind("LoadWorkerGuard::new()")
            .expect("the legacy GetResponse parse must be preceded by a guard");
        assert!(
            parse - guard_before < 200,
            "the legacy single-blob GetResponse parse (both arms) must be wrapped in a LoadWorkerGuard"
        );
    }

    /// freenet/river#397 Codex review 4: the authoritative current-delegate
    /// PerRoom load must (a) route its terminal through the pure `per_room_terminal`
    /// fed (map_empty, listed_count, had_fetch_error), and (b) record fetch
    /// failures both locally (had_fetch_error) and globally (mark_fetch_failure) in
    /// the three failure arms — but NOT for a definitive `value: None`.
    #[test]
    fn per_room_load_uses_terminal_and_marks_fetch_failure() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");

        // (a) the PerRoom terminal is the pure decision fed all three inputs, and
        // written through the ATTEMPT-SCOPED `worker.set_load_state` (review 7).
        assert!(
            production.contains("worker.set_load_state(per_room_terminal(")
                && production.contains("loaded_map_empty")
                && production.contains("listed_count")
                && production.contains("had_fetch_error"),
            "the PerRoom terminal must be worker.set_load_state(per_room_terminal(loaded_map_empty, listed_count, had_fetch_error))"
        );

        // (b) the per-room fetch loop's three failure arms mark both signals; the
        // value: None arm marks neither. Bound to the loop (marker → `let meta`).
        let loop_start = production
            .find("Loading {} per-room slot(s) from delegate")
            .expect("per-room fetch loop marker must exist");
        let loop_end = production[loop_start..]
            .find("let meta = if has_meta")
            .map(|i| loop_start + i)
            .expect("the per-room loop must be followed by the meta load");
        let loop_region = &production[loop_start..loop_end];
        assert_eq!(
            loop_region.matches("had_fetch_error = true;").count(),
            3,
            "the transport-Err, unexpected-response, and unparseable-slot arms must set had_fetch_error"
        );
        assert_eq!(
            loop_region.matches("mark_fetch_failure()").count(),
            3,
            "the same three arms must set the global SAW_FETCH_FAILURE"
        );
        let value_none_pos = loop_region
            .find("listed but value missing")
            .expect("the value: None arm must exist");
        let after_none =
            &loop_region[value_none_pos..(value_none_pos + 140).min(loop_region.len())];
        assert!(
            !after_none.contains("had_fetch_error = true;")
                && !after_none.contains("mark_fetch_failure()"),
            "a definitive `value: None` is a legitimate skip, not a fetch failure"
        );
    }

    /// freenet/river#417: the startup room load must fan the per-room delegate
    /// GETs out CONCURRENTLY — enqueue every request in a wave first (synchronous
    /// WS send), then await that wave's responses together via `join_all` —
    /// rather than awaiting each in a serial `for … .await`. That collapses
    /// wall-clock from N round-trips to ~ceil(N / ROOM_LOAD_CONCURRENCY). Pin the
    /// wiring by source-grep (this repo's convention), scoped to the per-room load
    /// region so the trailing single meta GET (which legitimately still uses
    /// `send_delegate_request`) is excluded.
    #[test]
    fn per_room_load_fans_out_concurrently() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");
        let start = production
            .find("Loading {} per-room slot(s) from delegate")
            .expect("per-room fetch loop marker must exist");
        let end = production[start..]
            .find("let meta = if has_meta")
            .map(|i| start + i)
            .expect("the per-room loop must be followed by the meta load");
        let region = &production[start..end];

        // Requests are ENQUEUED (send-only) and their responses awaited
        // CONCURRENTLY via join_all — not a combined serial send+await.
        let enqueue_pos = region
            .find("enqueue_delegate_request(")
            .expect("per-room GETs must be enqueued (send) before awaiting");
        let join_pos = region
            .find("join_all(")
            .expect("per-room responses must be awaited concurrently via join_all");
        assert!(
            region.contains("await_delegate_response("),
            "per-room responses must be awaited via the split await_delegate_response"
        );
        // Borrow-safety (the PR's one real landmine): the `WEB_API`-borrowing
        // enqueue must run BEFORE the concurrent await phase, and must NOT appear
        // INSIDE the join_all fan-out — otherwise two enqueues would interleave
        // across an await and double-borrow `WEB_API` (a RefCell panic in
        // single-threaded WASM). These two assertions pin exactly that ordering,
        // so a refactor to the tempting single-loop shape
        // `join_all(map(|vk| async { enqueue(vk).await?; await_response(...).await }))`
        // (which re-introduces the landmine) fails CI even though it still uses
        // enqueue + join_all.
        assert!(
            enqueue_pos < join_pos,
            "per-room GETs must be enqueued (WEB_API send) BEFORE the join_all await phase"
        );
        assert!(
            !region[join_pos..].contains("enqueue_delegate_request("),
            "the join_all fan-out must not contain an enqueue (WEB_API borrow) — \
             enqueues stay in the sequential per-wave loop to avoid a RefCell double-borrow"
        );
        // Guard against a regression to the serial combined send+await in the
        // per-room region (the trailing meta GET, outside this region, may still
        // use send_delegate_request).
        assert!(
            !region.contains("send_delegate_request("),
            "per-room load must not use the serial send_delegate_request in the fan-out region"
        );
        // The fan-out is bounded (does not fire all N at once) — it iterates
        // chunks of ROOM_LOAD_CONCURRENCY to stay under the node's delegate queue
        // cap (PR #419 review).
        assert!(
            region.contains("chunks(ROOM_LOAD_CONCURRENCY)"),
            "per-room load must fan out in bounded waves of ROOM_LOAD_CONCURRENCY, not all N at once"
        );
    }

    /// freenet/river#397 Codex review 4 (concurrency P2): the legacy re-save
    /// completion (in `hydrate_loaded_rooms`) must resolve `Loaded` only when it
    /// merged live rooms (`had_loaded_rooms`) — an empty legacy completion writes
    /// NOTHING so it can't stomp a concurrent probe's `Migrating` into a false
    /// Empty; the backstop owns the all-nothing terminal.
    #[test]
    fn legacy_resave_completion_gated_on_had_rooms() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");
        // Slice the legacy re-save closure (marker → the `had_loaded_rooms` return
        // that closes hydrate).
        let start = production
            .find("Migrating room data from legacy delegate to new delegate")
            .expect("legacy re-save marker must exist");
        let end = production[start..]
            .find("had_loaded_rooms\n}")
            .map(|i| start + i)
            .unwrap_or(production.len());
        let block = &production[start..end];
        // Both the Ok and Err arms resolve Loaded, each gated on `if had_loaded_rooms`.
        assert_eq!(
            block.matches("if had_loaded_rooms {").count(),
            2,
            "both legacy re-save arms must gate the Loaded write on had_loaded_rooms"
        );
        assert_eq!(
            block
                .matches("set_load_state_if_current(attempt, RoomsLoadState::Loaded)")
                .count(),
            2,
            "both legacy re-save arms resolve to Loaded (attempt-gated, review 11) when live rooms merged"
        );
    }

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

    /// A room's PRIVATE IDENTITY KEY survives a delegate WASM re-key
    /// (freenet/river#612).
    ///
    /// `RoomData::self_sk` IS the user's identity in a room. It is not derived
    /// from anything and cannot be regenerated — unlike the `signing_key:`
    /// secret, which `crate::signing::migrate_signing_key` re-derives from
    /// `self_sk` after every migration. If `self_sk` is lost, the room is lost.
    ///
    /// Nothing deliberately protects it. It survives only because it happens to
    /// sit inside the `RoomData` blob stored under the generic, INDEXED key
    /// `room:<b58 owner_vk>` — and the delegate's key index is what the
    /// migration probe's `ListRequest` enumerates. Move the key to a bespoke
    /// secret written the way `handle_store_signing_key` writes one (no
    /// `key_index` update, see `delegates/chat-delegate/src/handlers.rs`) and it
    /// leaves the indexed set, stops being listed, and stops migrating. River
    /// re-keys its delegate roughly weekly, so that lands as "every user's room
    /// identity is gone" at the next routine release, with no error raised
    /// anywhere.
    ///
    /// This test therefore drives the REAL code on both sides of the re-key
    /// rather than asserting the field exists on the struct:
    ///   * the predecessor's blob comes from `reconcile_room_present`, the sole
    ///     production writer of `room:<vk>`;
    ///   * discovery goes through the real `plan_load_from_keys`, which is what
    ///     turns a `ListResponse` into the set of slots to fetch;
    ///   * the successor's read is the same `from_reader::<RoomSlot>` +
    ///     `reconstruct_rooms` pair that `migrate_legacy_per_room` runs.
    ///
    /// Its companion below proves the negative, which is what makes this
    /// assertion load-bearing rather than a serde round-trip in disguise.
    #[test]
    fn self_sk_survives_a_legacy_delegate_migration() {
        let owner = vk(21);
        let identity = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut room = crate::room_data::test_minimal_room_data(owner);
        room.self_sk = Some(identity.clone());

        // (a) PREDECESSOR DELEGATE. The bytes the old delegate holds, produced
        // by the real per-room save chokepoint against an absent slot.
        let stored = crate::components::app::chat_delegate::reconcile_room_present(
            /* current */ None, &owner, &room, /* explicitly_rejoined */ false,
        )
        .expect("the save path must reconcile a fresh room")
        .expect("an absent slot stores fresh rather than aborting");

        // (b) DISCOVERY. The migration probe only ever sees what the delegate's
        // key index lists. Keys reach `migrate_legacy_per_room` through
        // `plan_load_from_keys`, so a room that does not plan as `PerRoom` here
        // is a room the migration never fetches at all.
        let listed = vec![ChatDelegateKey::new(room_storage_key(&owner))];
        assert_eq!(
            plan_load_from_keys(&listed),
            LoadPlan::PerRoom {
                room_vks: vec![owner],
                has_meta: false,
            },
            "the per-room key must be discoverable from the delegate's key \
             index — this is the whole reason the blob migrates"
        );

        // (c) SUCCESSOR DELEGATE. Exactly the decode + reconstruct that
        // `migrate_legacy_per_room` performs on each fetched slot.
        let slot: RoomSlot =
            from_reader(&stored[..]).expect("the migration must be able to decode the slot");
        let migrated = reconstruct_rooms(vec![(owner, slot)], None);

        // (d) The identity arrived on the far side, intact.
        let arrived = migrated
            .map
            .get(&owner)
            .expect("the room itself must migrate");
        assert_eq!(
            arrived.self_sk.as_ref().map(|k| k.to_bytes()),
            Some(identity.to_bytes()),
            "the room's PRIVATE identity key must survive the delegate re-key \
             verbatim — if this fails, every existing user loses their identity \
             in every room at the next release (freenet/river#612)"
        );
        assert_eq!(
            arrived.signing_key().map(|k| k.to_bytes()),
            Some(identity.to_bytes()),
            "and the accessor the rest of the UI signs through must find it"
        );
    }

    /// The negative half of the pin above (freenet/river#612): if `self_sk` were
    /// relocated out of the room blob into its own bespoke secret — the
    /// `signing_key:` pattern, and a genuinely reasonable-looking hardening
    /// move — the migration would NOT carry it, and would report success anyway.
    ///
    /// Without this test the positive one could pass for the wrong reason: a
    /// CBOR round-trip of a struct that still has the field would satisfy it
    /// even under the refactor that breaks production. Here the writer stores a
    /// blob shaped the way the relocation would shape it, everything else is
    /// held identical, and the same real migration reader runs — so the two
    /// tests together isolate "the key survives" to the one thing that actually
    /// causes it: `self_sk` being inside the indexed `room:<vk>` blob.
    ///
    /// Note what does NOT happen below: no decode error, no missing room, no
    /// failure signal of any kind. The room migrates, still knows WHO the user
    /// is (`self_vk` rides along), and has merely lost the ability to SIGN. That
    /// silence is the hazard — the migration cannot notice this and neither can
    /// the user, until the first send fails.
    #[test]
    fn a_relocated_self_sk_would_not_survive_a_legacy_delegate_migration() {
        // The room blob as a River that had relocated the private key would
        // write it: same room, same identity, `self_sk` simply absent. Mirrors
        // `room_data::tests::roomslot_decodes_from_a_blob_with_no_self_sk`.
        #[derive(serde::Serialize)]
        struct RelocatedRoomData {
            owner_vk: ed25519_dalek::VerifyingKey,
            room_state: river_core::room_state::ChatRoomStateV1,
            self_vk: ed25519_dalek::VerifyingKey,
            contract_key: freenet_stdlib::prelude::ContractKey,
        }
        #[derive(serde::Serialize)]
        enum RelocatedRoomSlot {
            Present(Box<RelocatedRoomData>),
            #[allow(dead_code)]
            Tombstone,
        }

        let owner = vk(22);
        let identity = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let room = crate::room_data::test_minimal_room_data(owner);

        let mut stored = Vec::new();
        ciborium::ser::into_writer(
            &RelocatedRoomSlot::Present(Box::new(RelocatedRoomData {
                owner_vk: owner,
                room_state: room.room_state.clone(),
                self_vk: identity.verifying_key(),
                contract_key: room.contract_key,
            })),
            &mut stored,
        )
        .unwrap();

        // The bespoke secret the relocation would introduce, alongside the room
        // blob. `handle_store_signing_key` never updates `key_index`, so in
        // production a key like this is not even LISTED by the migration probe.
        // Grant the refactor the more generous case anyway — pretend it somehow
        // reached the probe — and it still changes nothing: the loader has no
        // concept of this key and plans to fetch only the room slot.
        let bespoke = ChatDelegateKey::new(
            format!("self_sk:{}", bs58::encode(owner.to_bytes()).into_string()).into_bytes(),
        );
        assert_eq!(
            plan_load_from_keys(&[ChatDelegateKey::new(room_storage_key(&owner)), bespoke]),
            LoadPlan::PerRoom {
                room_vks: vec![owner],
                has_meta: false,
            },
            "a key outside the room:/rooms_meta/rooms_data family is invisible \
             to the loader even when listed — relocating the private key puts \
             it somewhere the migration has no way to ask for"
        );

        // The same real migration read as the positive test.
        let slot: RoomSlot = from_reader(&stored[..])
            .expect("the relocated blob still decodes — the room does not vanish");
        let migrated = reconstruct_rooms(vec![(owner, slot)], None);
        let arrived = migrated
            .map
            .get(&owner)
            .expect("the room still migrates, which is exactly why this is silent");

        assert!(
            arrived.self_sk.is_none(),
            "the private identity key did NOT cross the delegate re-key — this \
             is the regression freenet/river#612 exists to prevent, and the \
             reason the positive test above is a real pin and not a serde \
             round-trip"
        );
        assert!(
            arrived.signing_key().is_none(),
            "so nothing in the UI can sign for this room any more"
        );
        assert_eq!(
            arrived.self_verifying_key(),
            Some(identity.verifying_key()),
            "yet the room migrated and still names the user — no error is \
             raised on any path, which is what makes the loss silent"
        );
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

    /// freenet/river#527 (second cause): the signing key pushed to the delegate
    /// must be the identity that WON the merge, not the one this particular
    /// delegate response happened to carry.
    ///
    /// The extraction used to run BEFORE the (deferred) merge, straight off
    /// `loaded_rooms`. So when the merge kept a different identity for a room —
    /// or dropped it — this still pushed the losing key, and
    /// `migrate_signing_key` overwrites on mismatch. The delegate was left
    /// signing with an identity the UI had already rejected, which invalidates
    /// every signature it produces for that room.
    #[test]
    fn signing_key_migration_reads_the_merged_rooms_not_the_loaded_copy() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");

        let start = production
            .find("let loaded_keys_for_signing = loaded_keys;")
            .expect("the signing-key migration block must exist");
        let end = production[start..]
            .find("// Mark all loaded rooms as having completed initial sync")
            .expect("the block's end marker must exist");
        let block = &production[start..start + end];

        assert!(
            block.contains("crate::util::defer("),
            "the signing-key migration must be DEFERRED so it runs after the merge"
        );
        assert!(
            block.contains("ROOMS.peek()"),
            "it must read the MERGED rooms to learn which identity won"
        );
        assert!(
            !block.contains("loaded_rooms"),
            "it must NOT read the pre-merge loaded copy — that is the #527 bug: \
             pushing an identity the merge rejected over the good one"
        );
        // Anti-vacuity: match the actual CALL, not the words. The previous
        // needle ("migrate_signing_key") also matched a comment further down,
        // so deleting the real call left this pin green.
        let squeeze = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
        assert!(
            squeeze(block).contains(&squeeze(
                "crate::signing::migrate_signing_key(delegate_room_key, &signing_key, false)"
            )),
            "the block must still make the real migrate_signing_key call — otherwise              this pin is guarding nothing"
        );
        // The skip-on-absent is the actual mechanism: a room the merge DROPPED
        // (tombstoned, or an identity conflict resolved against this copy) must
        // yield no signing key at all. Turn this `?` into a fallback and the
        // second cause of #527 returns while every other assertion here holds.
        assert!(
            squeeze(block).contains(&squeeze("let room_data = rooms.map.get(vk)?;")),
            "a room absent from the merged map must be SKIPPED, not defaulted —              pushing a key for it is exactly the identity the merge rejected"
        );
    }

    /// freenet/river#590: a ranked resurrection must be drained into
    /// `mark_room_rejoined`, or the save path never learns of it.
    ///
    /// The merge restoring a room in memory is only half the fix. The save path
    /// is rank-blind by construction — `MergeRanks` is never persisted and
    /// `RoomSlot::Tombstone` carries no rank — so `reconcile_room_present` sees
    /// the delegate's tombstone, answers `AbortAdoptLeave`, and writes nothing.
    /// The room then lives for exactly one session and is gone for good, which is
    /// #590's symptom with an extra session in front of it.
    #[test]
    fn a_ranked_resurrection_is_drained_into_mark_room_rejoined() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");
        let body = production
            .find("fn hydrate_loaded_rooms_with_authority(")
            .map(|i| &production[i..])
            .expect("hydrate must exist");
        let body = &body[..body.find("\n}\n").expect("terminates")];
        // The drain must COLLECT OUT of the ranks closure, and the marking must
        // happen after that closure has closed. Textual `drain < mark` ordering
        // is NOT enough: calling `mark_room_rejoined` inside the closure also
        // satisfies it, and `mark_room_rejoined` takes the same non-reentrant
        // `Mutex` — which on single-threaded WASM deadlocks the tab rather than
        // failing a test.
        assert!(
            body.contains("let resurrected: Vec<[u8; 32]> ="),
            "the drain must collect OUT of the ranks closure into a local"
        );
        let drain = body
            .find("ranks.resurrected.drain()")
            .expect("the resurrection set must be drained after the merge");
        // Anchor the drain to the thing that POPULATES the set, not just to the
        // marking that consumes it. Hoisting the drain to the top of this
        // function — next to the `tombstoned` computation, which also takes the
        // ranks lock, so it is a natural place to consolidate the two
        // acquisitions — preserves every shape asserted here while draining an
        // EMPTY set. Nothing is ever marked, `reconcile_room_present` goes back
        // to `AbortAdoptLeave`, and #590 is silently restored.
        let merge = body
            .find("merge_from_source(")
            .expect("the merge that populates the set must be in this slice");
        assert!(
            merge < drain,
            "the drain must run AFTER the merge that populates the resurrection set"
        );
        let closure_end = body[drain..]
            .find("});")
            .map(|i| drain + i + 3)
            .expect("the ranks closure must close");
        let mark = body
            .find("mark_room_rejoined(vk)")
            .expect("and fed to mark_room_rejoined, which also clears ROOM_SLOT_STATE");
        assert!(
            mark > closure_end,
            "mark_room_rejoined must be called AFTER the ranks lock is released — \
             inside the closure it re-locks a non-reentrant Mutex and hangs the tab"
        );
    }

    /// freenet/river#527 — THE WIRING PIN.
    ///
    /// Every behavioural test for the fix lives at the `Rooms::merge_from_source`
    /// layer and supplies its own ranks map, so they all keep passing if the
    /// production CALL SITE stops feeding that layer real inputs. Two one-line
    /// edits revert the entire fix with green CI:
    ///
    ///   1. `with_identity_source_ranks(...)` -> `&mut HashMap::new()`. Every
    ///      conflict then reads `unwrap_or(SOURCE_RANK_AUTHORITATIVE)` for the
    ///      local rank, so nothing ever outranks it and merge keeps local
    ///      always — exactly the pre-fix behaviour.
    ///   2. the threaded `source_rank` -> a constant. Every generation then
    ///      ranks equal, equal ranks keep local, and first-responder-wins (the
    ///      original bug) is back.
    ///
    /// So pin the wiring itself: the shared registry, the threaded rank, and
    /// the per-generation rank at the legacy call site.
    #[test]
    fn hydrate_wires_the_shared_registry_and_a_per_generation_rank() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");
        let strip_ws = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
        let squeezed = strip_ws(production);

        // (1) The merge reads the SHARED, process-global rank registry — not a
        // throwaway map — so ranks recorded by one delegate generation's
        // hydration are visible to the next one's.
        assert!(
            squeezed.contains(&strip_ws(
                "chat_delegate::with_identity_source_ranks( |identity_ranks| {"
            )),
            "the merge must read the SHARED identity-source registry; a fresh map \
             per hydration makes every conflict keep local and silently reverts #527"
        );

        // (2) The rank passed to the merge is the THREADED parameter, and the
        // authority is derived from whether the RESPONDING delegate is legacy —
        // a hard-coded `Authoritative` here would let an older generation's
        // tombstone evict a live room again (freenet/river#590).
        assert!(
            squeezed.contains(&strip_ws(
                "current_rooms.merge_from_source( loaded_rooms, source_rank, authority, identity_ranks,)"
            )),
            "the merge must be given the threaded per-response source_rank and authority"
        );
        // The authority must be DERIVED in the wrapper and THREADED to the merge —
        // asserting the derivation exists somewhere in the file is not enough, it
        // has to be what actually reaches `merge_from_source`.
        let wrapper = production
            .find("fn hydrate_loaded_rooms(")
            .map(|i| &production[i..])
            .expect("hydrate_loaded_rooms must exist");
        let wrapper = &wrapper[..wrapper.find("\n}\n").expect("wrapper terminates")];
        assert!(
            strip_ws(wrapper).contains(&strip_ws(
                "let authority = if is_legacy_delegate { crate::room_data::MergeAuthority::OlderSnapshot } else { crate::room_data::MergeAuthority::Authoritative };"
            )),
            "the wrapper must derive the authority from is_legacy_delegate"
        );
        assert!(
            strip_ws(wrapper).contains(&strip_ws("hydrate_loaded_rooms_with_authority(")),
            "and pass it on rather than deriving it a second time downstream"
        );
        let inner = production
            .find("fn hydrate_loaded_rooms_with_authority(")
            .map(|i| &production[i..])
            .expect("the explicit form must exist");
        let inner = &inner[..inner.find("\n}\n").expect("terminates")];
        assert!(
            !inner.contains("if is_legacy_delegate {\n        crate::room_data::MergeAuthority"),
            "the merge must use the PASSED authority, not re-derive it — that would \
             silently override the blob caller's explicit OlderSnapshot"
        );

        // freenet/river#590 follow-up: the pre-merge `tombstoned` filter must ask
        // `MergeRanks` whether a `Present` at this rank survives, not just whether
        // the room is currently tombstoned. Ranking tombstones created a path
        // that did not exist before — a room can come BACK OUT of
        // `removed_rooms` — and a filter blind to it drops the restored room from
        // `room_keys`, so it is never subscribed, never synced, and its signing
        // key is never migrated. Rescued from deletion, then inert.
        let tomb = production
            .find("let tombstoned: std::collections::HashSet")
            .map(|i| &production[i..])
            .expect("the tombstone filter must exist");
        let tomb = &tomb[..tomb.find("\n    };").expect("filter terminates")];
        assert!(
            tomb.contains("presence_survives_tombstone("),
            "the filter must use the merge's own survival rule, or a room the \
             merge restores is dropped from the sync/subscribe loops"
        );
        assert!(
            strip_ws(production).contains(&strip_ws(
                "let incoming_rank = authority.rank_for(source_rank);"
            )),
            "and it must rank the response the same way the merge does"
        );

        // freenet/river#590: EVERY `hydrate_loaded_rooms` call site's
        // `is_legacy_delegate` argument, pinned by value. Flipping the legacy
        // per-room site from `true` to `false` restores #590 on the exact path it
        // was reported for — the derivation and threading pins above both stay
        // green, because they say nothing about what is fed IN.
        let sites: Vec<&str> = production
            .match_indices("hydrate_loaded_rooms(")
            .map(|(i, _)| {
                let tail = &production[i..];
                &tail[..tail
                    .find(')')
                    .map(|j| j + 1)
                    .unwrap_or(tail.len())
                    .min(tail.len())]
            })
            .filter(|c| !c.starts_with("hydrate_loaded_rooms(\n    loaded_rooms"))
            .collect();
        assert_eq!(
            sites.len(),
            3,
            "expected three hydrate_loaded_rooms call sites (the blob site uses \
             the explicit-authority form); got {sites:?}"
        );
        let legacy_sites = sites
            .iter()
            .filter(|c| strip_ws(c).contains("true,"))
            .count();
        assert_eq!(
            legacy_sites, 1,
            "exactly ONE call site is the legacy per-room migration and must pass \
             is_legacy_delegate = true; got {sites:?}"
        );

        // freenet/river#590: a `rooms_data` blob is a frozen snapshot living on
        // the current delegate. Classifying it `Authoritative` lets a tombstone it
        // carries evict a room the user has since rejoined, during the very
        // interrupted-migration recovery that re-reads it.
        let blob = production
            .find("async fn migrate_current_blob_to_per_room(")
            .map(|i| &production[i..])
            .expect("blob migration must exist");
        let blob = &blob[..blob.find("\n}\n").expect("terminates")];
        assert!(
            strip_ws(blob).contains(&strip_ws(
                "crate::room_data::MergeAuthority::OlderSnapshot,"
            )),
            "the current-delegate blob must merge as an OlderSnapshot"
        );

        // (3) The legacy per-room call site derives the rank from THAT
        // delegate's key, so different generations actually get different
        // ranks. A constant here collapses them and restores first-wins.
        assert!(
            squeezed.contains(&strip_ws(
                "source_rank_for_delegate_key(legacy_key.bytes())"
            )),
            "the legacy per-room hydrate must rank by the responding delegate's own key"
        );
        // And the single-blob path ranks by the responding delegate too.
        assert!(
            squeezed.contains(&strip_ws(
                "let delegate_source_rank = source_rank_for_delegate_key(key.bytes());"
            )),
            "the delegate-response path must capture the responding generation's rank"
        );
    }

    /// freenet/river#527 (third cause): a legacy migration seals only once the
    /// whole fan-out is quiescent.
    ///
    /// Sealing inside the re-save ended the migration as soon as the FIRST
    /// generation finished, while newer generations could still be answering.
    /// Close the tab in that window and the current delegate is left holding
    /// the older copy — non-empty, so no later session re-probes legacy — which
    /// is what made an identity rollback permanent rather than transient.
    #[test]
    fn legacy_resave_defers_the_seal_to_quiescence() {
        let src = include_str!("response_handler.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");

        let ok_arm = production
            .find("Successfully migrated room data to new delegate")
            .expect("legacy re-save success marker must exist");
        // Bounded to the Ok arm itself, before the Err arm's marker.
        let arm_end = production[ok_arm..]
            .find("Failed to migrate room data to new delegate")
            .expect("the Err arm marker must follow the Ok arm");
        let arm = &production[ok_arm..ok_arm + arm_end];

        assert!(
            arm.contains("request_legacy_seal_on_quiescence()"),
            "the legacy re-save must request a quiescence-gated seal"
        );
        assert!(
            !arm.contains("mark_legacy_migration_done("),
            "it must NOT seal inline — that ends the migration while newer \
             delegate generations may still be answering (freenet/river#527)"
        );
    }
}
