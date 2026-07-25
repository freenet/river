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
use crate::util::{owner_vk_to_contract_key, strip_upgrade_pointer, to_cbor_vec};
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
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfoV1};
use river_core::room_state::message::{AuthorizedMessageV1, RetentionHorizon};
use river_core::room_state::privacy::PrivacyMode;
use river_core::room_state::{
    ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta, ChatRoomStateV1Summary,
};
use std::collections::HashMap;
use std::sync::Arc;

/// CRDT-merge `incoming` into `local`, returning the delta that was applied
/// (`None` when the two were already in agreement).
///
/// This is `ComposableState::merge` unrolled, for one reason: the default
/// `merge` passes ONE `parent_state` to all three legs, and the three legs want
/// different things.
///
/// * `summarize` must see the SUMMARIZING peer's own state.
///   `MessagesV1::summarize` reads `max_recent_messages` from it to size its
///   retention horizon; hand it anything else and the horizon is wrong.
/// * `delta` deliberately reads nothing from it (the default `merge` would hand
///   the SENDER's `delta` the RECEIVER's state, so anything read there would be
///   the wrong peer's), but passing `incoming` keeps the argument honest.
/// * `apply_delta` ignores its outer `parent_state` entirely — the
///   `#[composable]` macro clones `self` per field — so the cheap default
///   sentinel stays, saving a full-state clone per network event
///   (freenet/river#246).
///
/// Returning the delta lets tests assert on what was actually pulled over,
/// which is the only way to catch a regression that produces the right final
/// state via the wrong amount of traffic.
fn merge_incoming_state(
    local: &mut ChatRoomStateV1,
    params: &ChatRoomParametersV1,
    incoming: &ChatRoomStateV1,
) -> Result<Option<ChatRoomStateV1Delta>, String> {
    let summary = local.summarize(local, params);
    let delta = incoming.delta(incoming, params, &summary);
    let parent_sentinel = ChatRoomStateV1::default();
    local.apply_delta(&parent_sentinel, params, &delta)?;
    Ok(delta)
}

fn compute_update_data(
    state: &ChatRoomStateV1,
    baseline: Option<&ChatRoomStateV1>,
    params: &ChatRoomParametersV1,
) -> Option<UpdateData<'static>> {
    // Sanitize the upgrade pointer OUT of every generic sync write
    // (freenet/river#427 P2-2). The ONLY legitimate upgrade publish is the
    // owner's pointer on the OLD contract, sent via a dedicated path in
    // `process_rooms`. A device that absorbed the owner's courtesy pointer while
    // on an older generation keeps it locally, and without this the composite
    // delta (or the full-state fallback) would re-emit that (now backward)
    // pointer onto the current contract on every sync tick, re-poisoning it. A
    // stripped `state.upgrade == None` makes `OptionalUpgradeV1::delta` return
    // None, so no upgrade sub-delta is ever emitted here.
    let state = strip_upgrade_pointer(state);
    if let Some(baseline) = baseline {
        let summary = outbound_summary(baseline, params);
        let delta = state.delta(baseline, params, &summary)?;
        Some(UpdateData::Delta(to_cbor_vec(&delta).into()))
    } else {
        Some(UpdateData::State(to_cbor_vec(&state).into()))
    }
}

/// The baseline summary for an OUTGOING update, with every retention horizon
/// neutralised.
///
/// # Why the horizons must not survive here
///
/// A retention horizon is a **receiver-published** quantity: it says "do not
/// offer me entries I would discard on arrival", and it is only meaningful
/// when the summary came from the peer that will apply the delta.
///
/// `compute_update_data`'s `baseline` is NOT a receiver. It is
/// `last_synced_state` — this device's own snapshot of what it last pushed.
/// Feeding its horizon to `delta` therefore filters the sender's outgoing
/// update against the SENDER's own retention window, which is meaningless at
/// best and lossy at worst:
///
/// `message.time` is the browser's wall clock (`get_current_system_time`), so
/// a device whose clock is behind by more than the room's retention window
/// composes messages whose `order_key()` sorts at or below its own baseline's
/// oldest retained key. Those get filtered OUT of the outgoing delta and are
/// never put on the wire at all. Worse, `compute_update_data` returning `None`
/// takes the `None` branch in `process_rooms`, which still calls
/// `sync_info.state_updated(..)` and advances the baseline — so the message is
/// never retried. It sits in the sender's own UI forever while no other peer
/// ever sees it.
///
/// Before the retention horizon existed, such a message went out and the
/// contract's own cap-prune decided its fate. Two things changed and both are
/// regressions: it is now dropped BEFORE the wire, losing the case where
/// canonical state is below cap and would have KEPT it; and the failure is
/// silent and purely local.
///
/// So the outbound path takes the id-set difference ONLY — exactly the
/// pre-horizon behaviour — and leaves retention to the contract, which is the
/// only party that knows the canonical state and the room's real cap. The
/// horizon still does its job on the RECEIVE path (`merge_incoming_state`),
/// which is where the summary genuinely belongs to the applying peer.
///
/// Pinned by `outbound_update_is_not_filtered_by_the_senders_own_horizon`.
fn outbound_summary(
    baseline: &ChatRoomStateV1,
    params: &ChatRoomParametersV1,
) -> ChatRoomStateV1Summary {
    // EXHAUSTIVE DESTRUCTURE, deliberately — do not replace this with
    // `let mut summary = ...; summary.x = ...`.
    //
    // This function is a hand-maintained mirror of "which summary fields are
    // horizon-shaped", and nothing else in the codebase links it to the horizon
    // definitions. Binding every field by name makes adding a field to
    // `ChatRoomStateV1Summary` a COMPILE ERROR *at exactly the site that has to
    // make the keep-or-clear decision*, instead of a silent default-to-keep.
    //
    // That is not hypothetical. `MembersV1` (`remove_excess_members`) and
    // `BansV1` (`max_user_bans`) have the same non-monotonic defect and are
    // deferred to a follow-up — see "Not fixed here". The moment that follow-up
    // adds `MembersSummary.horizon`, a struct-update spread here would compile
    // clean and silently reintroduce this exact bug on a new field: a device
    // withholding its own member records from every outgoing update, baseline
    // advancing anyway, never retried.
    let ChatRoomStateV1Summary {
        configuration,
        bans,
        members,
        member_info,
        secrets,
        mut recent_messages,
        mut direct_messages,
        upgrade,
        version,
    } = baseline.summarize(baseline, params);

    // The ONLY two horizon-shaped fields today. "Horizon-shaped" means the
    // field makes the SENDER withhold something it holds and the receiver
    // lacks; every other field is a pure have-statement (an id set, a version,
    // a signature map), which is safe — indeed required — to feed from the
    // sender's own baseline, because that is what makes the delta "what changed
    // since I last synced".
    recent_messages.horizon = RetentionHorizon::Open;
    direct_messages.pair_horizons.clear();

    ChatRoomStateV1Summary {
        configuration,
        bans,
        members,
        member_info,
        secrets,
        recent_messages,
        direct_messages,
        upgrade,
        version,
    }
}

/// Send a member-info self-heal as a standalone, member_info-only UPDATE.
///
/// This is the UPDATE-path counterpart to the GET-path self-heal in
/// `get_response.rs`. For a private room, a freshly-invited member's
/// invitation-accept PUT omits `member_info` when the room secret was not yet
/// available to seal the nickname (it cannot leak a plaintext nickname). The
/// secret normally arrives later via a subscription UPDATE, and once it does
/// `build_member_info_heal` can finally seal and publish the nickname — but the
/// GET-path heal never runs on the UPDATE path, so without this the member
/// stays "Unknown" to every other peer until the next full GET (app reload).
/// See freenet/river#295.
///
/// `heal_info` must already have been built (inside the `ROOMS.with_mut`
/// borrow, against the post-merge network state) and handed out here so the
/// send happens AFTER the borrow is released. The delta is self-signed and
/// idempotent: re-sending it is harmless if it raced another heal, and once
/// the entry lands the next `build_member_info_heal` returns `None`, so the
/// UPDATE stops firing.
///
/// `pub(crate)` so the identity-overwrite path (`members.rs::complete_identity_import`)
/// can trigger the heal too: an in-place identity swap does NO GET, so without
/// this the new identity would render "Unknown" until an unrelated future heal
/// (freenet/river#414, Codex round-6 P2-4).
pub(crate) fn send_member_info_heal_update(
    owner_vk: VerifyingKey,
    heal_info: AuthorizedMemberInfo,
) {
    let key = owner_vk_to_contract_key(&owner_vk);
    let heal_delta = ChatRoomStateV1Delta {
        member_info: Some(vec![heal_info]),
        ..Default::default()
    };
    let update_request = ContractRequest::Update {
        key,
        data: UpdateData::Delta(to_cbor_vec(&heal_delta).into()),
    };
    wasm_bindgen_futures::spawn_local(async move {
        if let Some(web_api) = WEB_API.write().as_mut() {
            match web_api
                .send(ClientRequest::ContractOp(update_request))
                .await
            {
                Ok(_) => info!(
                    "Sent member_info self-heal UPDATE (secret arrived via UPDATE) for room {:?}",
                    MemberId::from(owner_vk)
                ),
                Err(e) => warn!(
                    "Failed to send member_info self-heal for room {:?}: {}",
                    MemberId::from(owner_vk),
                    e
                ),
            }
        } else {
            // No socket — the heal is dropped. Harmless: it is idempotent and
            // re-evaluated on the next secret-bearing UPDATE or GET.
            warn!(
                "WebAPI not available — member_info self-heal for room {:?} skipped, \
                 will retry on the next GET",
                MemberId::from(owner_vk)
            );
        }
    });
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
        // freenet/river#295: populated inside with_mut when a newly-arrived
        // private-room secret lets us finally seal & publish our own
        // member_info. Sent as a standalone UPDATE AFTER the borrow releases.
        let mut pending_member_info_heal: Option<AuthorizedMemberInfo> = None;

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
                                // freenet/river#295: the secret that just
                                // arrived may be the one we were missing to
                                // seal our own nickname. If we're still
                                // stranded ("Unknown" — in `members` but
                                // absent from `member_info`), build the
                                // self-heal now against the post-merge state.
                                // Idempotent: returns `None` once the entry
                                // lands, so it only fires while genuinely
                                // stranded.
                                pending_member_info_heal =
                                    room_data.build_member_info_heal(&room_data.room_state);
                            }

                            // Re-derive actions_state with decrypted payloads
                            // (apply_delta only processes public actions). See #310.
                            room_data.rebuild_private_actions_state();
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

        // freenet/river#295: a private-room secret arrived in this delta and
        // let us seal our own member_info — publish it so we stop rendering as
        // "Unknown". Sent here, after the with_mut borrow released.
        if let Some(heal_info) = pending_member_info_heal {
            send_member_info_heal_update(owner_vk, heal_info);
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

                // Strip any absorbed upgrade pointer before this FORWARD seed PUT
                // onto the current contract, so an imported room that carried a
                // backward pointer does not poison the current generation
                // (freenet/river#427 P2-2).
                let wrapped_state = WrappedState::new(to_cbor_vec(&strip_upgrade_pointer(state)));

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
        // freenet/river#295 (full-state path): same shape as the delta-path
        // local in apply_delta_inner — a newly-arrived private-room secret may
        // let us finally seal & publish our own member_info.
        let mut pending_member_info_heal: Option<AuthorizedMemberInfo> = None;
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
                // existing one.
                //
                // This used to pass a cheap `ChatRoomStateV1::default()`
                // sentinel as `parent_state`, on the (then-true) premise that
                // every field's `summarize`/`delta` declared the arg
                // `_parent_state`. That premise is DEAD: `MessagesV1::summarize`
                // now reads `max_recent_messages` from it to size the retention
                // horizon. Under the sentinel it would read the DEFAULT cap
                // instead of the room's, understate the horizon, and re-open the
                // resend loop the horizon exists to close.
                //
                // `merge_uses_room_state_as_parent_so_horizon_is_correct` pins
                // this. The `apply_delta` leg still takes the sentinel — see the
                // `apply_delta_inner` call site — because the macro ignores its
                // outer `_parent_state` there and clones `self` per field.
                match merge_incoming_state(
                    &mut room_data.room_state,
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
                                // freenet/river#295: see the matching comment
                                // in apply_delta_inner. The secret that just
                                // arrived may let us finally seal our own
                                // nickname and stop rendering as "Unknown".
                                pending_member_info_heal =
                                    room_data.build_member_info_heal(&room_data.room_state);
                            }

                            // Re-derive actions_state with decrypted payloads
                            // (merge only processes public actions). See #310.
                            room_data.rebuild_private_actions_state();
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

        // freenet/river#295 (full-state path): publish the self-heal built
        // above once a private-room secret arrived via this state update.
        // Symmetric with the apply_delta_inner path.
        if let Some(heal_info) = pending_member_info_heal {
            send_member_info_heal_update(room_owner_vk, heal_info);
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

    /// Every `ChatRoomStateV1Delta` field that is `Some`, by name.
    ///
    /// The outbound tests build `current` and `baseline` differing in EXACTLY
    /// one field, so the whole delta must be exactly that one field. Asserting
    /// per-field instead would miss the failure mode where `outbound_summary`
    /// over-clears some OTHER field: e.g. adding `summary.bans.clear()` makes
    /// every outgoing update re-send the full ban list forever, and a test that
    /// only reads `delta.recent_messages` never notices.
    fn populated_delta_fields(d: &ChatRoomStateV1Delta) -> Vec<&'static str> {
        [
            ("configuration", d.configuration.is_some()),
            ("bans", d.bans.is_some()),
            ("members", d.members.is_some()),
            ("member_info", d.member_info.is_some()),
            ("secrets", d.secrets.is_some()),
            ("recent_messages", d.recent_messages.is_some()),
            ("direct_messages", d.direct_messages.is_some()),
            ("upgrade", d.upgrade.is_some()),
            ("version", d.version.is_some()),
        ]
        .into_iter()
        .filter(|(_, populated)| *populated)
        .map(|(name, _)| name)
        .collect()
    }

    /// Populate `members` and `bans` so the whole-delta assertion in the
    /// outbound tests actually has something to bite on.
    ///
    /// Without this, `create_test_room()` leaves both empty, so over-clearing
    /// `summary.bans` in `outbound_summary` produces no ban delta (there are no
    /// bans to offer) and the assertion passes vacuously — which is exactly what
    /// happened on the first attempt at that assertion.
    fn add_members_and_bans(state: &mut ChatRoomStateV1, owner_sk: &SigningKey) {
        use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
        use river_core::room_state::member::{AuthorizedMember, Member, MembersV1};
        use std::time::Duration;

        let owner_id = MemberId::from(&owner_sk.verifying_key());
        let mut members = Vec::new();
        for seed in [41u8, 42, 43] {
            let sk = SigningKey::from_bytes(&[seed; 32]);
            members.push(AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: owner_id,
                    member_vk: sk.verifying_key(),
                },
                owner_sk,
            ));
        }
        let banned = MemberId::from(&SigningKey::from_bytes(&[44u8; 32]).verifying_key());
        state.members = MembersV1 { members };
        state.bans = BansV1(vec![AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(4_000),
                banned_user: banned,
            },
            owner_id,
            owner_sk,
        )]);
    }

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

    /// Build an owner-signed upgrade pointer to `target` (for #427 P2-2 tests).
    fn owner_upgrade_pointer(
        owner_sk: &SigningKey,
        target: [u8; 32],
    ) -> river_core::room_state::upgrade::OptionalUpgradeV1 {
        use river_core::room_state::upgrade::{AuthorizedUpgradeV1, OptionalUpgradeV1, UpgradeV1};
        let upgrade = UpgradeV1 {
            owner_member_id: MemberId::from(&owner_sk.verifying_key()),
            version: 1,
            new_chatroom_address: blake3::Hash::from(target),
        };
        OptionalUpgradeV1(Some(AuthorizedUpgradeV1::new(upgrade, owner_sk)))
    }

    /// P2-2 (freenet/river#427): the composite sync DELTA never carries an
    /// upgrade pointer, even when the local state absorbed one — otherwise every
    /// sync tick would re-emit the (now backward) pointer onto the current
    /// contract, re-poisoning it. Real changes (the new message) still ship.
    #[test]
    fn compute_update_data_delta_omits_upgrade_pointer() {
        let (baseline, params, owner_sk) = create_test_room();
        let mut current = baseline.clone();
        add_message(&mut current, &owner_sk, "hello");
        current.upgrade = owner_upgrade_pointer(&owner_sk, [9u8; 32]);

        let data = compute_update_data(&current, Some(&baseline), &params)
            .expect("a changed state must yield an update");
        let UpdateData::Delta(bytes) = data else {
            panic!("expected a delta");
        };
        let delta: ChatRoomStateV1Delta =
            ciborium::de::from_reader(bytes.as_ref()).expect("delta must decode");
        assert!(
            delta.upgrade.is_none(),
            "the composite sync delta must NOT carry an upgrade pointer (#427 P2-2)"
        );
        assert!(
            delta.recent_messages.is_some(),
            "the real change (the new message) must still ship in the delta"
        );
    }

    /// P2-2: the full-state sync fallback (no baseline) also strips the upgrade
    /// pointer, while preserving the rest of the state.
    #[test]
    fn compute_update_data_state_omits_upgrade_pointer() {
        let (mut current, params, owner_sk) = create_test_room();
        add_message(&mut current, &owner_sk, "hello");
        current.upgrade = owner_upgrade_pointer(&owner_sk, [9u8; 32]);

        let data = compute_update_data(&current, None, &params).expect("full state");
        let UpdateData::State(bytes) = data else {
            panic!("expected a full state");
        };
        let state: ChatRoomStateV1 =
            ciborium::de::from_reader(bytes.as_ref()).expect("state must decode");
        assert!(
            state.upgrade.0.is_none(),
            "the full-state sync must NOT carry an upgrade pointer (#427 P2-2)"
        );
        assert_eq!(
            state.recent_messages.messages.len(),
            1,
            "the message must survive the strip"
        );
    }

    /// P2-2: the shared strip helper clears the pointer and leaves the rest of
    /// the state intact.
    #[test]
    fn strip_upgrade_pointer_clears_pointer() {
        let (mut current, _params, owner_sk) = create_test_room();
        add_message(&mut current, &owner_sk, "hi");
        current.upgrade = owner_upgrade_pointer(&owner_sk, [9u8; 32]);
        assert!(current.upgrade.0.is_some());

        let cleaned = strip_upgrade_pointer(&current);
        assert!(cleaned.upgrade.0.is_none());
        assert_eq!(cleaned.recent_messages.messages.len(), 1);
    }

    /// P2-2 source pin (freenet/river#427): the forward-write discipline in
    /// `process_rooms`. The imported-room seed PUT strips the pointer, while the
    /// ONE legitimate publish — the owner's pointer on the OLD contract — is
    /// left intact. A future refactor dropping the strip, or accidentally
    /// stripping the owner publish, would silently re-regress. We cannot drive
    /// the async signal path in a unit test, so this is a source-text pin
    /// (mirrors `apply_delta_inner_revives_hidden_thread_for_inbound_dm_sender`).
    #[test]
    fn forward_write_upgrade_pointer_discipline_pinned() {
        let src = include_str!("room_synchronizer.rs");
        assert!(
            src.contains("to_cbor_vec(&strip_upgrade_pointer(state))"),
            "the imported-room seed PUT must strip the upgrade pointer (#427 P2-2)"
        );
        assert!(
            src.contains("upgrade: Some(authorized_upgrade)"),
            "the owner-only OLD-contract upgrade delta must still publish the pointer \
             (the one legitimate publish — #427 P2-2)"
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

    /// Pin that the incoming-state merge path hands `summarize` the ROOM's
    /// own state, not a cheap `ChatRoomStateV1::default()` sentinel.
    ///
    /// # History
    ///
    /// `update_room_state_inner` used to call
    /// `room_state.merge(&ChatRoomStateV1::default(), &params, &incoming)`,
    /// saving a full-state clone per network event (the freenet/river#246
    /// follow-up). That was sound only while EVERY field's `summarize`/`delta`
    /// declared its `parent_state` argument unused — which the predecessor of
    /// this test asserted.
    ///
    /// It is no longer true. `MessagesV1::summarize` reads
    /// `max_recent_messages` off `parent_state` to size its retention horizon,
    /// the mechanism that stops a peer being offered messages it would
    /// immediately prune. Under the sentinel it reads the DEFAULT cap (100)
    /// instead of the room's, so a room at a smaller cap advertises an OPEN
    /// horizon, and the sender ships a window of messages the room drops on
    /// arrival — on every fan-out, forever. That is the loop the horizon
    /// exists to close.
    ///
    /// # Discrimination design
    ///
    /// The final STATE is identical either way (the extra messages are pruned
    /// straight back out), so asserting on post-merge state cannot catch this
    /// — that is exactly how the regression would slip through. The assertion
    /// is therefore on the DELTA `merge_incoming_state` returns: with the
    /// correct parent it must be empty, with the sentinel it is not. Verified
    /// by mutation: swapping `local.summarize(local, params)` back to
    /// `local.summarize(&ChatRoomStateV1::default(), params)` fails this test.
    #[test]
    fn merge_uses_room_state_as_parent_so_horizon_is_correct() {
        use river_core::room_state::configuration::AuthorizedConfigurationV1;

        let (mut room_state, params, owner_sk) = create_test_room();

        // A cap well below the default of 100, so a sentinel parent reads a
        // materially different value.
        let mut cfg = room_state.configuration.configuration.clone();
        cfg.max_recent_messages = 5;
        room_state.configuration = AuthorizedConfigurationV1::new(cfg, &owner_sk);

        // Explicit timestamps rather than `add_message`'s `SystemTime::now()`:
        // the whole point is a strict older/newer split, and two `now()` calls
        // can land on the same instant, which would make the ordering fall
        // through to the (random) message-id tiebreak and the test flaky.
        let at = |state: &mut ChatRoomStateV1, secs: u64, body: &str| {
            let msg = MessageV1 {
                room_owner: state.configuration.configuration.owner_member_id,
                author: MemberId::from(&owner_sk.verifying_key()),
                content: RoomMessageBody::public(body.to_string()),
                time: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
            };
            state
                .recent_messages
                .messages
                .push(AuthorizedMessageV1::new(msg, &owner_sk));
        };

        // The peer's window is strictly older: three messages the room has
        // never seen, every one below anything the room retains.
        let mut incoming = room_state.clone();
        for i in 0..3u64 {
            at(&mut incoming, 1_000 + i, &format!("older-{i}"));
        }
        for i in 0..5u64 {
            at(&mut room_state, 2_000 + i, &format!("newer-{i}"));
        }

        // Premise: the room is at capacity and the peer really does hold
        // messages it lacks, or the assertion below is vacuous.
        assert_eq!(
            room_state.recent_messages.messages.len(),
            5,
            "test premise: the room must be exactly at its cap"
        );
        let held: Vec<_> = room_state
            .recent_messages
            .messages
            .iter()
            .map(|m| m.id())
            .collect();
        assert!(
            incoming
                .recent_messages
                .messages
                .iter()
                .any(|m| !held.contains(&m.id())),
            "test premise: the incoming state must hold messages the room lacks"
        );

        let before = room_state.recent_messages.messages.clone();
        let applied =
            merge_incoming_state(&mut room_state, &params, &incoming).expect("merge must succeed");

        assert!(
            applied
                .as_ref()
                .and_then(|d| d.recent_messages.as_ref())
                .is_none(),
            "the merge pulled over messages the room prunes on arrival —              `summarize` was given the wrong parent_state, so the retention              horizon was computed against the DEFAULT max_recent_messages              instead of this room's. See MessagesV1::RetentionHorizon."
        );
        assert_eq!(
            room_state.recent_messages.messages, before,
            "and, consistently, the retained window must be untouched"
        );
    }

    /// Pin the OTHER leg: `apply_delta` may still be handed a cheap
    /// `ChatRoomStateV1::default()` sentinel.
    ///
    /// # Why this needs its own test
    ///
    /// `merge_uses_room_state_as_parent_so_horizon_is_correct` above pins the
    /// `summarize` leg. It cannot pin this one: it drives
    /// `merge_incoming_state` on both sides of its assertion, and
    /// `merge_incoming_state` ALWAYS passes the sentinel to `apply_delta`, so
    /// the sentinel becoming unsafe is invisible to it. This test therefore
    /// calls `apply_delta` directly, once with each parent.
    ///
    /// # The invariant, and what breaks if it goes
    ///
    /// The `#[composable]` macro's generated `apply_delta` takes
    /// `_parent_state` and shadows it with a per-field `self.clone()`
    /// (`freenet-scaffold-macro/src/lib.rs`, whose own comment calls this
    /// "ugly" — a description, not a stability promise). Two live call sites
    /// bet on it: `merge_incoming_state` and the `apply_delta_inner` path.
    ///
    /// If a freenet-scaffold bump ever forwarded `_parent_state`, both UI
    /// ingestion paths would apply every delta against a DEFAULT state:
    /// `members` empty, so `MessagesV1::apply_delta`'s author-must-be-a-member
    /// retain drops EVERY message; `bans` empty; `max_members` and
    /// `max_recent_messages` at their defaults rather than the room's. Silent,
    /// total, local message loss — with no error anywhere.
    ///
    /// The room here deliberately carries a non-default `max_members`,
    /// `max_recent_messages` and a real ban, because those are exactly the
    /// `parent_state` fields the per-field `apply_delta`s read.
    #[test]
    fn apply_delta_ignores_its_outer_parent_state_so_the_sentinel_is_safe() {
        use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
        use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
        use river_core::room_state::member::{AuthorizedMember, Member, MembersV1};
        use std::time::Duration;

        let owner_sk = SigningKey::generate(&mut rand::thread_rng());
        let owner_vk = owner_sk.verifying_key();
        let owner_id = MemberId::from(&owner_vk);
        let params = ChatRoomParametersV1 { owner: owner_vk };

        let member_sks: Vec<SigningKey> = (0..3)
            .map(|_| SigningKey::generate(&mut rand::thread_rng()))
            .collect();
        let authorize = |sk: &SigningKey| {
            AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: owner_id,
                    member_vk: sk.verifying_key(),
                },
                &owner_sk,
            )
        };

        // Every one of these is deliberately NOT the default.
        let cfg = Configuration {
            owner_member_id: owner_id,
            max_recent_messages: 4,
            max_members: 3,
            max_message_size: 500,
            max_user_bans: 5,
            ..Default::default()
        };

        let banned_id = MemberId::from(&member_sks[2].verifying_key());
        let ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(9_000),
                banned_user: banned_id,
            },
            owner_id,
            &owner_sk,
        );

        let msg_at = |sk: &SigningKey, secs: u64, body: &str| {
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: owner_id,
                    author: MemberId::from(&sk.verifying_key()),
                    content: RoomMessageBody::public(body.to_string()),
                    time: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
                },
                sk,
            )
        };

        let mut local = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(cfg, &owner_sk),
            members: MembersV1 {
                members: vec![authorize(&member_sks[0]), authorize(&member_sks[1])],
            },
            bans: BansV1(vec![ban]),
            ..Default::default()
        };
        for i in 0..3u64 {
            local.recent_messages.messages.push(msg_at(
                &member_sks[0],
                2_000 + i,
                &format!("held-{i}"),
            ));
        }

        // An incoming peer with more members and more messages, so the delta
        // is non-trivial across several fields.
        let mut incoming = local.clone();
        incoming.members.members.push(authorize(&member_sks[2]));
        for i in 0..4u64 {
            incoming.recent_messages.messages.push(msg_at(
                &member_sks[1],
                3_000 + i,
                &format!("new-{i}"),
            ));
        }

        let delta = incoming.delta(&incoming, &params, &local.summarize(&local, &params));
        assert!(
            delta.is_some(),
            "test premise: the incoming state must actually produce a delta"
        );

        // --- the assertion: both parents must give byte-identical results ---
        let mut via_sentinel = local.clone();
        via_sentinel
            .apply_delta(&ChatRoomStateV1::default(), &params, &delta)
            .expect("sentinel-parent apply");

        let mut via_self = local.clone();
        let self_parent = via_self.clone();
        via_self
            .apply_delta(&self_parent, &params, &delta)
            .expect("self-parent apply");

        assert_eq!(
            via_sentinel, via_self,
            "applying the same delta against the sentinel parent and against the \
             room's own state produced DIFFERENT states. The #[composable] macro \
             has stopped ignoring its outer `parent_state`, so `merge_incoming_state` \
             and `apply_delta_inner` are now merging every room against a DEFAULT \
             state — empty members, empty bans, default caps. Fix those call sites \
             to pass the room's own state (a clone) before this ships."
        );

        // --- proof the assertion above is not vacuous ---
        //
        // The two parents are NOT interchangeable to a field that genuinely
        // reads its `parent_state`. `MessagesV1::apply_delta` does, so calling
        // it directly shows the divergence the room WOULD get if the macro
        // forwarded the argument. Without this, the assertion above would still
        // pass if the two parents happened to be equivalent, and would be
        // pinning nothing.
        let msg_delta = delta
            .as_ref()
            .and_then(|d| d.recent_messages.clone())
            .expect("test premise: the delta must carry messages");

        let mut msgs_sentinel = local.recent_messages.clone();
        msgs_sentinel
            .apply_delta(
                &ChatRoomStateV1::default(),
                &params,
                &Some(msg_delta.clone()),
            )
            .expect("messages under sentinel parent");
        let mut msgs_real = local.recent_messages.clone();
        msgs_real
            .apply_delta(&local, &params, &Some(msg_delta))
            .expect("messages under the real parent");

        assert!(
            msgs_sentinel.messages.is_empty(),
            "premise: under a DEFAULT parent the members list is empty, so the \
             author-must-be-a-member retain must drop every message — that is \
             the silent data loss this pin exists to prevent"
        );
        assert_ne!(
            msgs_sentinel.messages, msgs_real.messages,
            "the two parents must be distinguishable to a field that reads \
             parent_state, or the equality assertion above pins nothing"
        );
    }

    /// A locally-retained message must reach the wire even when its timestamp
    /// sorts at or below the BASELINE's oldest-retained key.
    ///
    /// # The bug this pins
    ///
    /// `compute_update_data` used `baseline.summarize(baseline, params)`
    /// directly. `baseline` is `last_synced_state` — this device's own
    /// snapshot, NOT a receiver — so the retention horizon inside it filtered
    /// the device's outgoing update against a STALE view of its own retention
    /// window. Anything sorting at or below that horizon was dropped from the
    /// delta and never put on the wire; `compute_update_data` then returned
    /// `None`, which in `process_rooms` still calls
    /// `sync_info.state_updated(..)` and advances the baseline, so it was never
    /// retried either.
    ///
    /// # Reachability — narrower than first described, and worth stating
    ///
    /// An earlier version of this comment (and of the commit that introduced
    /// the fix) claimed a clock-skewed device would simply never send anything.
    /// **That is wrong and is corrected here.** The UI send path applies the
    /// composed message through `room_data.room_state.apply_delta` before any
    /// sync tick (`conversation.rs`), and `MessagesV1::apply_delta` sorts by
    /// `(time, id)` and drains from the FRONT — so on a device already AT
    /// capacity, a message sorting below the local window is dropped LOCALLY at
    /// compose time and never reaches `compute_update_data` at all.
    ///
    /// The reachable trigger needs `current` to be BELOW cap while `baseline`
    /// was AT cap publishing a real horizon. Two routes:
    ///
    /// * `max_recent_messages` is RAISED, so back-filled older messages are
    ///   retained locally but still sort below the stale horizon. Modelled
    ///   here.
    /// * `post_apply_cleanup`'s ban/member sweep shrinks the local set while
    ///   `last_synced_state` still holds the pre-sweep snapshot.
    ///
    /// This test models the first route rather than an unreachable state: the
    /// baseline is at its cap of 3 and publishes a real `OldestRetained`, and
    /// `current` has the cap raised to 10 and holds a back-filled older message
    /// below that horizon. The configuration bump rides along in the delta,
    /// which is why the whole-delta assertion expects two fields.
    #[test]
    fn outbound_update_is_not_filtered_by_the_senders_own_horizon() {
        use river_core::room_state::configuration::AuthorizedConfigurationV1;
        use std::time::Duration;

        let (mut room_state, params, owner_sk) = create_test_room();
        add_members_and_bans(&mut room_state, &owner_sk);

        let msg_at = |secs: u64, body: &str| {
            AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: room_state.configuration.configuration.owner_member_id,
                    author: MemberId::from(&owner_sk.verifying_key()),
                    content: RoomMessageBody::public(body.to_string()),
                    time: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
                },
                &owner_sk,
            )
        };

        // BASELINE: cap 3, exactly 3 messages, so it publishes a real horizon.
        let mut baseline = room_state.clone();
        let mut cfg = baseline.configuration.configuration.clone();
        cfg.max_recent_messages = 3;
        baseline.configuration = AuthorizedConfigurationV1::new(cfg.clone(), &owner_sk);
        for i in 0..3u64 {
            baseline
                .recent_messages
                .messages
                .push(msg_at(5_000 + i, &format!("synced-{i}")));
        }
        assert!(
            matches!(
                baseline
                    .recent_messages
                    .retention_horizon(baseline.configuration.configuration.max_recent_messages),
                river_core::room_state::message::RetentionHorizon::OldestRetained(_)
            ),
            "test premise: the baseline must be at capacity and publish a real horizon"
        );

        // CURRENT: the room raised its cap, so a back-filled older message is
        // now retained locally even though it sorts below the stale horizon.
        let mut current = baseline.clone();
        let mut raised = cfg;
        raised.max_recent_messages = 10;
        raised.configuration_version += 1;
        current.configuration = AuthorizedConfigurationV1::new(raised, &owner_sk);
        let backfilled = msg_at(1_000, "back-filled-below-the-stale-horizon");
        let newer_a = msg_at(9_001, "newer-a");
        let newer_b = msg_at(9_002, "newer-b");
        current.recent_messages.messages.push(backfilled.clone());
        current.recent_messages.messages.push(newer_a.clone());
        current.recent_messages.messages.push(newer_b.clone());

        // Premise: `current` is genuinely BELOW its cap, so this state is
        // reachable through `apply_delta` — the earlier version of this test
        // held 6 messages against a cap of 3, which `apply_delta` can never
        // produce.
        assert!(
            current.recent_messages.messages.len()
                <= current.configuration.configuration.max_recent_messages,
            "test premise: `current` must be below its own cap, or the state is unreachable"
        );

        let update = compute_update_data(&current, Some(&baseline), &params)
            .expect("a locally-retained message must produce an outgoing update");

        let bytes = match update {
            UpdateData::Delta(d) => d.into_bytes(),
            other => panic!("expected a delta, got {other:?}"),
        };
        let delta: ChatRoomStateV1Delta =
            ciborium::de::from_reader(bytes.as_slice()).expect("decode outgoing delta");

        let mut sent: Vec<_> = delta
            .recent_messages
            .as_ref()
            .map(|m: &Vec<AuthorizedMessageV1>| m.iter().map(|m| m.id()).collect())
            .unwrap_or_default();
        sent.sort();
        assert!(
            sent.contains(&backfilled.id()),
            "the locally-RETAINED message was filtered out of the OUTGOING update by \
             this device's OWN stale horizon. `compute_update_data` would then return \
             None, and the `None` branch in process_rooms advances the baseline, so it \
             is never retried. See `outbound_summary`."
        );

        // The other direction, which is how this fix would go wrong:
        // neutralising the horizon must NOT widen the update into a resend of
        // everything already synced. Exact set equality, not a count.
        let mut expected = vec![backfilled.id(), newer_a.id(), newer_b.id()];
        expected.sort();
        assert_eq!(
            sent, expected,
            "the outgoing update must be EXACTLY the messages the baseline lacks. \
             Anything more means clearing the horizon has widened it into a resend \
             of already-synced state; anything less means something is still \
             filtering the send path."
        );

        // ...and exactly the fields that genuinely differ: the messages, plus
        // the configuration bump that raised the cap. Any OTHER populated field
        // means `outbound_summary` over-cleared something and every sync tick
        // now re-sends it.
        assert_eq!(
            populated_delta_fields(&delta),
            vec!["configuration", "recent_messages"],
            "the outgoing update touched fields the baseline already has"
        );
    }

    /// The DM half of the same fix — `pair_horizons.clear()` in
    /// `outbound_summary`.
    ///
    /// This needs its own test because the messages-side test above asserts
    /// only on `delta.recent_messages`. Deleting the single line
    /// `summary.direct_messages.pair_horizons.clear()` leaves that test, and
    /// every other test in the repo, green — while a clock-skewed device's DM
    /// to an at-capacity pair is silently dropped before the wire. That is the
    /// exact failure class the messages side was just fixed for, one line away.
    ///
    /// `DmPairHorizon` is published per ordered `(sender, recipient)` pair that
    /// has reached `MAX_DM_MESSAGES_PER_PAIR`, so it is a receiver-published
    /// quantity for precisely the same reason and must not survive onto the
    /// outbound path either.
    #[test]
    fn outbound_update_is_not_filtered_by_the_senders_own_dm_pair_horizon() {
        use river_core::room_state::direct_messages::{
            sign_direct_message, MAX_DM_MESSAGES_PER_PAIR,
        };

        let (mut room_state, params, owner_sk) = create_test_room();
        add_members_and_bans(&mut room_state, &owner_sk);

        let sender_sk = SigningKey::generate(&mut rand::thread_rng());
        let sender_id = MemberId::from(&sender_sk.verifying_key());
        let recipient_id = MemberId::from(&owner_sk.verifying_key());

        let dm_at = |ts: u64, tag: u8| {
            sign_direct_message(
                &sender_sk,
                sender_id,
                recipient_id,
                &params.owner,
                ts,
                vec![tag; 8],
            )
            .expect("sign dm")
        };

        // Fill the ordered pair to capacity so the baseline publishes a real
        // `DmPairHorizon`. Without this the property is vacuous — a pair below
        // the cap publishes no entry and nothing could be filtered.
        for i in 0..MAX_DM_MESSAGES_PER_PAIR {
            room_state
                .direct_messages
                .messages
                .push(dm_at(5_000 + i as u64, 1));
        }
        let baseline = room_state.clone();
        assert_eq!(
            baseline.direct_messages.pair_horizons().len(),
            1,
            "test premise: the pair must be AT capacity so a horizon is published"
        );

        // DMs this device just composed. `skewed`'s clock-derived timestamp
        // lands below everything the baseline's pair retains; the other is an
        // ordinary newer one, so the assertion below is a real set comparison.
        let skewed = dm_at(1_000, 2);
        let newer = dm_at(9_001, 3);
        let mut current = room_state.clone();
        current.direct_messages.messages.push(skewed.clone());
        current.direct_messages.messages.push(newer.clone());

        let update = compute_update_data(&current, Some(&baseline), &params)
            .expect("a newly composed DM must produce an outgoing update");
        let bytes = match update {
            UpdateData::Delta(d) => d.into_bytes(),
            other => panic!("expected a delta, got {other:?}"),
        };
        let delta: ChatRoomStateV1Delta =
            ciborium::de::from_reader(bytes.as_slice()).expect("decode outgoing delta");

        let sent_dms = delta
            .direct_messages
            .as_ref()
            .map(|d| d.new_messages.clone())
            .unwrap_or_default();
        assert!(
            sent_dms
                .iter()
                .any(|m| m.sender_signature == skewed.sender_signature),
            "the locally-composed DM was filtered out of the OUTGOING update by this \
             device's OWN pair horizon. `pair_horizons` is receiver-published, exactly \
             like the messages horizon, so it must be cleared in `outbound_summary` — \
             otherwise a clock-skewed device silently never delivers a DM to any pair \
             it holds at capacity."
        );
        // Exact set equality, for the same reason as the messages test: this is
        // the direction the fix would go wrong in. `outbound_summary` clears
        // only `pair_horizons` and keeps `message_signatures`, so the
        // signature-set difference still bounds the payload.
        let mut sent_sigs: Vec<_> = sent_dms.iter().map(|m| m.sender_signature).collect();
        sent_sigs.sort_by_key(|s| s.to_bytes());
        let mut expected = vec![skewed.sender_signature, newer.sender_signature];
        expected.sort_by_key(|s| s.to_bytes());
        assert_eq!(
            sent_sigs, expected,
            "the outgoing update must be EXACTLY the DMs the baseline lacks — not the \
             whole at-capacity pair, which is what clearing the pair horizon would \
             widen it to if the signature-set difference stopped applying."
        );

        assert_eq!(
            populated_delta_fields(&delta),
            vec!["direct_messages"],
            "the outgoing update touched fields the baseline already has"
        );
    }
}
