use crate::components::app::freenet_api::backward_probe::{
    advance_backward_probe, start_backward_probe, take_probe,
};
use crate::components::app::freenet_api::error::SynchronizerError;
use crate::components::app::freenet_api::response_handler::update_notification::{
    clear_upgrade_target, follow_upgrade_pointer_if_needed, upgrade_target_owner,
};
use crate::components::app::freenet_api::room_synchronizer::RoomSynchronizer;
use crate::components::app::notifications::mark_initial_sync_complete;
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{CURRENT_ROOM, PENDING_INVITES, ROOMS, WEB_API};
use crate::constants::ROOM_CONTRACT_WASM;
use crate::invites::PendingRoomStatus;
use crate::room_data::RoomData;
use crate::util::{
    from_cbor_slice, get_current_system_time, owner_vk_to_contract_key, to_cbor_vec,
    try_from_cbor_slice,
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
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::sync::Arc;

pub async fn handle_get_response(
    _room_synchronizer: &mut RoomSynchronizer,
    key: ContractKey,
    _contract: Vec<u8>,
    state: Vec<u8>,
) -> Result<(), SynchronizerError> {
    info!("Received get response for key {key}");

    // Backward-probe routing (freenet/river#292): a GET response whose
    // contract id matches an outstanding legacy-key probe is for a
    // *previous* room-contract generation. It cannot be resolved to an
    // owner via SYNC_INFO / ROOMS (both keyed by the CURRENT contract
    // id), so route it into the probe handler before any other lookup.
    if crate::components::app::freenet_api::backward_probe::is_probe_instance(key.id()) {
        handle_probe_get_response(key, state).await;
        return Ok(());
    }

    // First try to find the owner_vk from SYNC_INFO
    let owner_vk = SYNC_INFO.read().get_owner_vk_for_instance_id(key.id());

    // Upgrade-pointer-target fallback (freenet/river#292 multi-hop): a
    // GET response for a contract reached by following an upgrade
    // pointer is keyed by a NEWER contract id that isn't in SYNC_INFO or
    // ROOMS. Resolve it via the upgrade-target side-table.
    let owner_vk = owner_vk.or_else(|| upgrade_target_owner(key.id()));

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

        // Multi-hop upgrade chain (freenet/river#292, Task 1): if the
        // contract whose state we just GET'd carries an `OptionalUpgradeV1`
        // pointer to a *newer* contract, follow it — and because the GET
        // response for that newer contract is itself routed back through
        // here, a pointer→pointer→pointer chain is walked to its end.
        // `follow_upgrade_pointer_if_needed` carries the cycle / hop-cap
        // guards. We pass `key.id()` as `delivered_from` so a pointer
        // that targets the contract that just delivered it is not
        // chased into an infinite loop.
        //
        // Decode defensively (`try_from_cbor_slice`): this state can come
        // from a cross-generation pointer target whose `ChatRoomStateV1`
        // layout the current type cannot decode — same hazard the backward
        // probe guards against. An undeserializable state simply means
        // "no upgrade pointer to follow", so the chain terminates here.
        if let Some(probed_state) = try_from_cbor_slice::<ChatRoomStateV1>(&state) {
            follow_upgrade_pointer_if_needed(&probed_state, &owner_vk, Some(*key.id()));
        }
        // This GET response consumed any outstanding upgrade-target
        // side-table entry for this contract id.
        clear_upgrade_target(key.id());

        if is_pending_invite {
            info!("This is a subscription for a pending invitation, adding state");
            let retrieved_state: ChatRoomStateV1 = from_cbor_slice::<ChatRoomStateV1>(&state);

            // Structural backstop for freenet/river#365 (this issue, #367):
            // short-circuit the full new-member accept when the user is
            // ALREADY a member of this room under the per-room identity they
            // already hold (`RoomData.self_sk`).
            //
            // Every invitation carries a freshly generated
            // `invitee_signing_key`, so accepting one always introduces a NEW
            // key/MemberId. If we reach here for a room the user is already in
            // — e.g. the accept-time guard in
            // `receive_invitation_modal::accept_invitation` fell through
            // because `ROOMS.try_read()` was momentarily unreadable, or some
            // future caller populated `PENDING_INVITES` for an existing room —
            // running the full accept would:
            //   * PUT a DUPLICATE member entry (`build_state_for_put`),
            //   * overwrite `self_sk` with the fresh key, ORPHANING the
            //     original membership (the original #365 mechanism), and
            //   * record the fresh key's `self_authorized_member` /
            //     `self_member_info` (`record_invite_credentials`).
            //
            // A partial backstop that guarded ONLY the `self_sk` overwrite was
            // rejected by external review on PR #366: it left `self_sk` and the
            // recorded self-credentials pointing at DIFFERENT keys — a split,
            // internally inconsistent local state. The correct fix is to
            // short-circuit the ENTIRE accept here, BEFORE `build_state_for_put`:
            // treat it as a no-op refresh (merge the retrieved state,
            // repopulate secrets, clear the pending invite) so nothing —
            // membership, credentials, or `self_sk` — is mutated toward the
            // fresh key.
            //
            // This is best-effort in the same sense as the accept-time guard:
            // it only fires when `ROOMS` is readable. If `ROOMS` is genuinely
            // unreadable we fall through to the normal accept (the prior
            // behavior for that rare case). A genuine rejoin after leaving is
            // unaffected — `leave_room` drops the room from `ROOMS`, so the
            // held `self_sk` is no longer a member and this guard does not fire.
            //
            // Membership is judged against the freshly-fetched CANONICAL
            // `retrieved_state`, not the local ROOMS snapshot. The local
            // snapshot only supplies WHICH key we hold (`self_sk`); whether
            // that key is currently a member is decided by the authoritative
            // network state. Judging it from the local snapshot alone would
            // wrongly suppress a legitimate rejoin when that snapshot is STALE:
            // if the user was pruned for inactivity or removed/banned remotely,
            // local ROOMS may still list their held key while the canonical
            // state no longer does. In that case the user genuinely needs to
            // publish the invitation's fresh key, so we must fall through to
            // the normal accept. (Codex review of this PR.)
            let held_self_vk = ROOMS.try_read().ok().and_then(|rooms| {
                rooms
                    .map
                    .get(&owner_vk)
                    .map(|rd| rd.self_sk.verifying_key())
            });
            let already_member_under_held_key = held_self_vk.is_some_and(|self_vk| {
                held_key_is_member(&owner_vk, &retrieved_state.members.members, &self_vk)
            });
            if already_member_under_held_key {
                info!(
                    "GET response for pending invite to room {:?}: already a member under \
                     the held self_sk — refreshing instead of re-joining (freenet/river#367)",
                    MemberId::from(owner_vk)
                );

                // Refresh-only: merge the retrieved network state into the
                // existing RoomData and repopulate secrets, exactly as the
                // existing-room refresh path does. Do NOT touch `self_sk`,
                // membership, or self-credentials. Deferred (like every other
                // ROOMS mutation in this handler) to avoid a borrow conflict
                // with the synchronous signal reads above.
                crate::util::defer(move || {
                    ROOMS.with_mut(|rooms| {
                        if let Some(room_data) = rooms.map.get_mut(&owner_vk) {
                            let params = ChatRoomParametersV1 { owner: owner_vk };
                            let current_state = room_data.room_state.clone();
                            match room_data.room_state.merge(
                                &current_state,
                                &params,
                                &retrieved_state,
                            ) {
                                Ok(_) => {
                                    room_data.capture_self_membership_data(&params);
                                    let _ = room_data.repopulate_secrets_from_state();
                                }
                                Err(e) => {
                                    error!(
                                        "Re-accept refresh: failed to merge state for room \
                                         {:?}: {}",
                                        MemberId::from(owner_vk),
                                        e
                                    );
                                }
                            }
                        }
                    });
                });

                // Move the room out of `Subscribing` in SYNC_INFO.
                // `process_rooms()` set `RoomSyncStatus::Subscribing` before
                // sending this pending-invite GET (with `subscribe: false`);
                // the refresh path neither PUTs nor subscribes, so without
                // intervention the room would sit in `Subscribing` until the
                // retry/timeout machinery fired.
                //
                // We reset to `Disconnected`, NOT `Subscribed`: this GET never
                // established a subscription, and the room may genuinely not be
                // subscribed yet (e.g. a stale pending invite surviving a
                // reload/reconnect, or an imported room). `Disconnected` is the
                // status `rooms_awaiting_subscription` keys on, so
                // `process_rooms`'s existing-room path will establish a real
                // PUT+subscribe if needed; that re-PUT is idempotent and
                // self-healing if the room was in fact already subscribed.
                // Marking `Subscribed` here would instead suppress that path
                // and could leave an unsubscribed room silently not receiving
                // updates. (Codex review of this PR.)
                crate::util::defer(move || {
                    SYNC_INFO.with_mut(|sync_info| {
                        sync_info.update_sync_status(&owner_vk, RoomSyncStatus::Disconnected);
                    });
                });

                // Clear the now-moot pending invite. Mark it Subscribed so the
                // modal's terminal-success path (`render_subscribed_state`)
                // closes it and persistently dismisses the invitation, matching
                // a normal completed accept.
                crate::util::defer(move || {
                    PENDING_INVITES.with_mut(|pending| {
                        if let Some(join) = pending.map.get_mut(&owner_vk) {
                            join.status = PendingRoomStatus::Subscribed;
                        }
                    });
                });

                return Ok(());
            }

            // Get the pending invite data once to avoid multiple reads
            let (self_sk, authorized_member, preferred_nickname, room_secrets) = {
                let pending_invites = PENDING_INVITES.read();
                let invite = &pending_invites.map[&owner_vk];
                (
                    invite.invitee_signing_key.clone(),
                    invite.authorized_member.clone(),
                    invite.preferred_nickname.clone(),
                    invite.room_secrets.clone(),
                )
            };

            // The room secrets the inviter embedded in the invitation
            // (private rooms only). These let the invitee read the room —
            // and seal their own nickname — immediately, before the owner
            // delegate's `encrypted_secrets` back-fill arrives. Empty for
            // public rooms and pre-feature invitations.
            //
            // Drop any entry whose version exceeds the room's current
            // version — a malicious inviter could otherwise pad the
            // invitation with unbounded bogus future-version secrets.
            let invitation_secrets: std::collections::HashMap<u32, [u8; 32]> = {
                let current_version = retrieved_state.secrets.current_version;
                room_secrets
                    .into_iter()
                    .filter(|(version, _)| *version <= current_version)
                    .collect()
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
            // A PRIVATE room's nickname must be encrypted. The room secret
            // normally comes from the invitation itself (`room_secrets`),
            // so the nickname can be sealed right here; `seal_invitee_nickname`
            // also falls back to a blob the owner has already back-filled
            // into the contract. Only if NEITHER source has the secret do
            // we produce NO member_info — rather than fall back to a
            // plaintext `SealedBytes::public` seal, which would leak a
            // cleartext nickname into a private room. `build_member_info_heal`
            // then re-publishes it, properly sealed, on a later GET once the
            // secret has arrived. (The heal runs only on GET, not on the
            // UPDATE that typically delivers the secret — tracked as issue
            // #295.)
            let authorized_member_info: Option<AuthorizedMemberInfo> = {
                // Seal the invitee's nickname against the room secret —
                // taken from the contract state if the owner has already
                // back-filled this member's blob, otherwise from the
                // invitation-provided secret so the nickname lands in the
                // join PUT immediately (no "Unknown member" window).
                let sealed_nickname = crate::room_data::seal_invitee_nickname(
                    &retrieved_state,
                    &self_sk,
                    &invitation_secrets,
                    &preferred_nickname,
                );
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
                            deputies: Vec::new(),
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
                    // freenet/river#247. Also record the explicit rejoin so the
                    // per-room save overwrites a remote `Tombstone` slot with
                    // `Present` rather than adopting the leave
                    // (freenet/river#345 round-9).
                    rooms.removed_rooms.remove(&owner_vk);
                    crate::components::app::chat_delegate::mark_room_rejoined(owner_vk);

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
                            self_nickname: None,
                            previous_contract_key: None,
                            invitation_secrets: std::collections::HashMap::new(),
                        }
                    });

                    // Clear previous_contract_key on successful GET — proves migration worked
                    if room_data.previous_contract_key.is_some() {
                        room_data.previous_contract_key = None;
                    }

                    // If the room already existed, merge state and then decide
                    // whether to adopt the invitation's signing key.
                    if !is_new_entry {
                        // Create parameters for merge
                        let params = ChatRoomParametersV1 { owner: owner_vk };

                        // Clone current state to avoid borrow issues during merge
                        let current_state = room_data.room_state.clone();

                        // Merge the retrieved state into the existing state
                        // FIRST, so the membership decision below sees the
                        // CANONICAL members (the merge folds in any remote
                        // removal/ban). Judging it from the pre-merge local
                        // snapshot would be stale.
                        room_data
                            .room_state
                            .merge(&current_state, &params, &retrieved_state)
                            .expect("Failed to merge room states");

                        // Adopt the invitation's signing key as our per-room
                        // identity ONLY when the key we currently hold is not
                        // already a valid member of this room. Blindly
                        // overwriting `self_sk` orphans a live membership: the
                        // original member entry stays in the room, but the local
                        // user no longer holds its key, so they can never act as
                        // — or remove — it. That is the freenet/river#365
                        // mechanism. The accept-time guard in
                        // `receive_invitation_modal::accept_invitation` already
                        // blocks the user-facing re-accept path; the early
                        // short-circuit at the top of this branch is the
                        // structural backstop that makes the orphaning
                        // impossible regardless of how this GET was triggered.
                        // The owner is implicitly covered (the owner is always a
                        // member of their own room), preserving the previous
                        // "never strip owner privileges" behavior.
                        //
                        // This is judged against the MERGED state (canonical
                        // members), not the stale pre-merge local snapshot: if
                        // the held key was pruned/banned remotely, the merge has
                        // dropped it, so `existing_is_member` is correctly false
                        // and we adopt the fresh invitation key — a genuine
                        // rejoin. Keeping the old `self_sk` while the PUT
                        // publishes the fresh member and `record_invite_credentials`
                        // records the fresh key's credentials would split the
                        // local identity (self_sk vs credentials at different
                        // keys). (Codex review of this PR.)
                        let existing_vk = room_data.self_sk.verifying_key();
                        let existing_is_member = existing_vk == owner_vk
                            || room_data
                                .room_state
                                .members
                                .members
                                .iter()
                                .any(|m| m.member.member_vk == existing_vk);
                        if !existing_is_member {
                            room_data.self_sk = self_sk.clone();
                            // Reset migration flag so the new key gets migrated
                            room_data.key_migrated_to_delegate = false;
                        }
                    }

                    // Seed the secrets recovered from the invitation so a
                    // brand-new invitee can decrypt the room before the
                    // owner delegate's `encrypted_secrets` back-fill
                    // arrives. `extend` covers both the freshly-inserted
                    // entry and a pre-existing one (a re-accepted invite).
                    // `repopulate_secrets_from_state` below folds these
                    // into the in-memory `secrets` map.
                    room_data
                        .invitation_secrets
                        .extend(invitation_secrets.iter().map(|(k, v)| (*k, *v)));

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

                    // Store the accepted invitation's credentials for the
                    // rejoin and member-info self-heal paths. `self_nickname`
                    // in particular is what lets the heal restore the user's
                    // chosen nickname when `authorized_member_info` is `None`
                    // (a private room whose secret had not yet arrived to
                    // seal it) — by heal time the join-time `PendingRoomJoin`
                    // is long discarded.
                    room_data.record_invite_credentials(
                        authorized_member.clone(),
                        authorized_member_info.clone(),
                        preferred_nickname.clone(),
                    );
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
                    if room_data.is_private() {
                        // Re-derive with decrypted private action payloads (#310).
                        room_data.rebuild_private_actions_state();
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
                            // AUTHORITATIVE: accepting an invitation chooses this
                            // room's identity now (freenet/river#414 P1).
                            crate::signing::migrate_signing_key(room_key, &signing_key_clone, true)
                                .await;
                        if result != crate::signing::MigrationResult::Failed {
                            // Must defer signal mutations from spawn_local to
                            // avoid RefCell already borrowed panics in Dioxus runtime
                            crate::util::defer(move || {
                                let mut sanitized = false;
                                ROOMS.with_mut(|rooms| {
                                    if let Some(room_data) = rooms.map.get_mut(&owner_vk) {
                                        // Only mark migrated for the identity that
                                        // actually completed: an overwrite may have
                                        // replaced `self_sk` while this migration ran,
                                        // and marking the room migrated for a
                                        // superseded key would strand the new identity
                                        // (freenet/river#414).
                                        if room_data.self_sk != signing_key_clone {
                                            return;
                                        }
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
            // Backward-probe recovery (freenet/river#292, Task 3).
            //
            // The current contract key is always authoritative. If THIS
            // GET — for the room's CURRENT contract key — came back with
            // no real state (a default `ChatRoomStateV1` whose
            // configuration signature does not verify against the
            // owner), the room may be stranded under an older
            // room-contract generation. Probe the legacy keys
            // newest-to-oldest before doing anything else.
            //
            // We gate strictly on "current key" so we never probe in
            // response to a GET that was itself for a legacy/pointer
            // contract. `is_probe_instance` already short-circuited
            // legacy-key responses at the top of this function; the
            // explicit equality check here is defence-in-depth.
            let retrieved_state: ChatRoomStateV1 = from_cbor_slice::<ChatRoomStateV1>(&state);

            let is_current_key = key.id() == owner_vk_to_contract_key(&owner_vk).id();
            let retrieved_has_real_state = retrieved_state
                .configuration
                .verify_signature(&owner_vk)
                .is_ok();

            if is_current_key && !retrieved_has_real_state {
                info!(
                    "Current contract key for room {:?} returned empty/default state — \
                     starting backward probe (freenet/river#292)",
                    MemberId::from(owner_vk)
                );
                // Capture the device's local snapshot now and hand it to the
                // probe. It rides along in `ProbeState` so the probe can both
                // CRDT-merge it with any recovered state and use it for the
                // last-resort seed — without a fallible signal read at
                // probe-completion time.
                let local_snapshot = ROOMS
                    .read()
                    .map
                    .get(&owner_vk)
                    .map(|rd| rd.room_state.clone())
                    .unwrap_or_default();
                if start_backward_probe(owner_vk, local_snapshot) {
                    // A probe is in flight. It is responsible for either
                    // recovering stranded state (CRDT-merge + PUT-forward)
                    // or, on full exhaustion, seeding the current key with
                    // the local snapshot. Do NOT seed here — that would
                    // race the probe and re-introduce the stale state
                    // this whole mechanism exists to avoid.
                    return Ok(());
                }
                // `false` => no legacy generations exist. There is
                // nothing to recover, so fall through to the normal
                // path, which seeds the current key with the local
                // snapshot (the genuine last resort).
                info!(
                    "No legacy generations for room {:?} — falling through to seed current key",
                    MemberId::from(owner_vk)
                );
            }

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
            // Member-info self-heal — remediation for the PR #272 stranding.
            // If the freshly-GET'd canonical state shows us in `members`
            // with no matching `member_info` entry, every other peer
            // renders us as "Unknown". Detect it from the raw network
            // state and re-publish our own self-signed member_info.
            // Idempotent: once the entry lands, later GETs no longer see
            // the stranding and the heal stops firing.
            // (`retrieved_state` was parsed above by the #292 backward-
            // probe logic.)
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
                            // Note: for an imported room the member-info heal
                            // entry was folded into `retrieved_state_for_put`
                            // (the PUT payload), NOT into `retrieved_state`.
                            // So this local `room_state` briefly lacks the
                            // heal entry — self renders as "Unknown" locally
                            // until the post-PUT subscription delivers the
                            // network state back. Transient and self-healing.
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
            //
            // The heal is NOT written into the local `room_state` here,
            // so self renders as "Unknown" locally until the subscription
            // notification echoes this UPDATE back — the same benign,
            // self-resolving transient as the imported-room path above.
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
                    } else {
                        // No socket — the heal is dropped. Harmless: it is
                        // idempotent and re-evaluated on the next GET, but
                        // log it so a stranded user isn't a silent mystery.
                        warn!(
                            "WebAPI not available — member_info self-heal for room \
                             {:?} skipped, will retry on the next GET",
                            MemberId::from(owner_vk)
                        );
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
                                // HYDRATION: re-migrating the already-stored
                                // `self_sk` read from ROOMS after the imported
                                // room's GET-first sync — NOT a new identity
                                // choice, so it must not override the registry and
                                // is discarded if superseded (freenet/river#414 P1).
                                crate::signing::migrate_signing_key(room_key, &self_sk, false)
                                    .await;
                            if result != crate::signing::MigrationResult::Failed {
                                let migrated_key = self_sk.clone();
                                crate::util::defer(move || {
                                    ROOMS.with_mut(|rooms| {
                                        if let Some(room_data) = rooms.map.get_mut(&owner_vk) {
                                            // Only mark migrated for the identity that
                                            // actually completed: an overwrite may have
                                            // replaced `self_sk` while this migration ran,
                                            // and marking the room migrated + sanitizing
                                            // for a superseded key would strand the new
                                            // identity (freenet/river#414, round-10 P2 —
                                            // the guard the other 3 callbacks already have).
                                            if room_data.self_sk != migrated_key {
                                                return;
                                            }
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

/// Handle a GET response whose contract id matches an outstanding
/// backward probe (freenet/river#292, Task 3).
///
/// The GET was for a *legacy* room-contract generation. Two outcomes:
///
/// * **The legacy key holds real state** (its `configuration` signature
///   verifies against the owner): this is the room's last-active
///   stranded state. CRDT-merge it into the room's `RoomData` in `ROOMS`
///   and PUT it forward onto the CURRENT contract key so the room is no
///   longer stranded. The probe ends here.
/// * **The legacy key is empty/default**: advance the probe to the next
///   (older) legacy key. If the probe is now exhausted — every
///   generation tried, nothing found — seed the CURRENT contract key
///   with the device's local snapshot as the genuine last resort.
pub(crate) async fn handle_probe_get_response(key: ContractKey, state: Vec<u8>) {
    let Some(probe) = take_probe(key.id()) else {
        // Race: the probe entry was already consumed — e.g. the real GET
        // response and the timeout watchdog both fired. Whichever ran first
        // owns the outcome; the loser lands here. Nothing to do.
        warn!(
            "Probe GET response for {} had no matching probe entry — ignoring",
            key.id()
        );
        return;
    };
    let owner_vk = probe.owner_vk;

    // Deserialize defensively: the probe walks many historical generations,
    // and a very old one may carry a `ChatRoomStateV1` whose layout the
    // current type cannot decode. Treat an undeserializable legacy state as
    // empty (default) so the probe advances to the next generation rather
    // than panicking — matching the CLI's `try_get_state` (freenet/river#292).
    let recovered_state: ChatRoomStateV1 = match try_from_cbor_slice::<ChatRoomStateV1>(&state) {
        Some(s) => s,
        None => {
            warn!(
                "Backward probe: legacy contract {} returned undeserializable state \
                 (incompatible older generation) — treating as empty",
                key.id()
            );
            ChatRoomStateV1::default()
        }
    };
    let recovered_has_real_state = recovered_state
        .configuration
        .verify_signature(&owner_vk)
        .is_ok();

    if recovered_has_real_state {
        info!(
            "Backward probe for room {:?} recovered real state from legacy contract {} \
             — adopting and PUTting forward onto the current key",
            MemberId::from(owner_vk),
            key.id()
        );

        // No need to follow this recovered state's own upgrade pointer: the
        // probe walks generations newest-first, so every generation newer than
        // this one was already probed and found empty. The newest-first probe
        // order subsumes forward upgrade-pointer following.
        //
        // CRDT-merge the recovered state with the device's local snapshot
        // BEFORE PUTting it forward, so unsynced local edits the device made
        // offline are not dropped from the migrating PUT. This matches the
        // CLI's `get_migration_state`, which merges network + local.
        let params = ChatRoomParametersV1 { owner: owner_vk };
        let merged_state = merge_room_states(recovered_state, &probe.local_snapshot, &params);

        // Adopt the merged state into the room's RoomData. If the room is
        // still a placeholder (a fresh import), replace wholesale — exactly
        // as the normal imported-room GET path does, because the placeholder's
        // `owner_member_id` differs and `merge` would reject it.
        let rooms_state = merged_state.clone();
        crate::util::defer(move || {
            ROOMS.with_mut(|rooms| {
                if let Some(room_data) = rooms.map.get_mut(&owner_vk) {
                    let params = ChatRoomParametersV1 { owner: owner_vk };
                    if room_data.is_awaiting_initial_sync() {
                        room_data.room_state = rooms_state;
                        room_data.capture_self_membership_data(&params);
                        let _ = room_data.repopulate_secrets_from_state();
                    } else {
                        let current_state = room_data.room_state.clone();
                        match room_data
                            .room_state
                            .merge(&current_state, &params, &rooms_state)
                        {
                            Ok(_) => {
                                room_data.capture_self_membership_data(&params);
                                let _ = room_data.repopulate_secrets_from_state();
                            }
                            Err(e) => {
                                error!(
                                    "Backward probe: failed to merge recovered state \
                                     for room {:?}: {}",
                                    MemberId::from(owner_vk),
                                    e
                                );
                            }
                        }
                    }
                } else {
                    warn!(
                        "Backward probe recovered state for room {:?} but it is no \
                         longer in ROOMS — discarding",
                        MemberId::from(owner_vk)
                    );
                }
            });
        });

        // PUT the merged state forward onto the current contract key so the
        // room is no longer stranded, and subscribe to it.
        put_state_to_current_key(owner_vk, merged_state, "recovered (backward probe)").await;

        crate::util::defer(move || {
            SYNC_INFO.with_mut(|sync_info| {
                sync_info.register_new_room(owner_vk);
                sync_info.update_sync_status(&owner_vk, RoomSyncStatus::Subscribing);
            });
            // Enable notifications — the room now has real state, exactly
            // as the normal imported-room GET path does once it has
            // adopted network state.
            mark_initial_sync_complete(&owner_vk);
        });
        crate::components::app::mark_needs_sync(owner_vk);
        return;
    }

    // Legacy key empty — advance to the next older generation. Capture the
    // local snapshot first so the exhaustion seed below has it without a
    // fallible signal read (the probe is consumed by `advance_backward_probe`).
    let local_snapshot = probe.local_snapshot.clone();
    if advance_backward_probe(probe) {
        info!(
            "Backward probe for room {:?}: legacy contract {} empty, advancing",
            MemberId::from(owner_vk),
            key.id()
        );
        return;
    }

    // Probe exhausted — no legacy generation held real state. Last resort:
    // seed the current contract key with the device's local snapshot. This is
    // the ONLY path on which the local snapshot is PUT onto the network (the
    // core design principle of #292). The snapshot was captured up front, so
    // this seed always runs — it never depends on a signal read here.
    info!(
        "Backward probe for room {:?} exhausted — seeding current contract key \
         with the local snapshot (last resort)",
        MemberId::from(owner_vk)
    );
    put_state_to_current_key(owner_vk, local_snapshot, "local snapshot (last resort)").await;
    crate::util::defer(move || {
        SYNC_INFO.with_mut(|sync_info| {
            sync_info.register_new_room(owner_vk);
            sync_info.update_sync_status(&owner_vk, RoomSyncStatus::Subscribing);
        });
    });
}

/// CRDT-merge `local` into `primary`, returning the merged state. On a merge
/// failure (genuinely incompatible states) returns `primary` unchanged rather
/// than losing it.
fn merge_room_states(
    primary: ChatRoomStateV1,
    local: &ChatRoomStateV1,
    params: &ChatRoomParametersV1,
) -> ChatRoomStateV1 {
    let mut merged = primary.clone();
    match merged.merge(&primary, params, local) {
        Ok(()) => merged,
        Err(e) => {
            warn!(
                "Backward probe: merge of recovered + local state failed ({e}); \
                 using recovered state alone"
            );
            primary
        }
    }
}

/// PUT `state` onto the room's CURRENT contract key (the key derived
/// from the owner VK + currently-bundled WASM) with `subscribe: true`.
///
/// Used by the backward probe both to push recovered stranded state
/// forward and to seed the current key with the local snapshot when no
/// stranded state was found.
async fn put_state_to_current_key(
    owner_vk: ed25519_dalek::VerifyingKey,
    state: ChatRoomStateV1,
    label: &str,
) {
    let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
    let parameters = ChatRoomParametersV1 { owner: owner_vk };
    let params_bytes = to_cbor_vec(&parameters);
    let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
        WrappedContract::new(Arc::new(contract_code), Parameters::from(params_bytes)),
    ));
    let wrapped_state = WrappedState::new(to_cbor_vec(&state));

    let put_request = ContractRequest::Put {
        contract: contract_container,
        state: wrapped_state,
        related_contracts: Default::default(),
        subscribe: true,
        blocking_subscribe: false,
    };

    if let Some(web_api) = WEB_API.write().as_mut() {
        match web_api.send(ClientRequest::ContractOp(put_request)).await {
            Ok(_) => info!(
                "Backward probe: PUT {} state to current contract for room {:?}",
                label,
                MemberId::from(owner_vk)
            ),
            Err(e) => error!(
                "Backward probe: failed to PUT {} state for room {:?}: {}",
                label,
                MemberId::from(owner_vk),
                e
            ),
        }
    } else {
        error!(
            "Backward probe: WebAPI unavailable, cannot PUT {} state for room {:?}",
            label,
            MemberId::from(owner_vk)
        );
    }
}

/// Is the per-room identity the user already holds (`self_vk`, the
/// verifying key of `RoomData.self_sk`) already a member of this room —
/// either as the owner, or present in the member list?
///
/// This is the predicate the freenet/river#367 short-circuit in
/// `handle_get_response` uses to decide a re-accept should be a no-op
/// refresh rather than a second join. It mirrors `vk_is_room_member` in
/// `receive_invitation_modal` (the accept-time guard's predicate) so the
/// two layers agree on what "already a member" means: the structural
/// backstop must recognize exactly the rooms the entry-level guard would
/// have caught, regardless of how the GET was triggered.
///
/// Note this checks the HELD `self_vk`, never the invitation's freshly
/// generated `invitee_signing_key` (which is never a member yet) — checking
/// only the latter is the original #365 mistake.
///
/// Pure (no signal access, no WASM) so it is unit-testable without
/// constructing a full `RoomData`, whose `contract_key` needs the
/// room-contract WASM — the same split `vk_is_room_member` uses.
pub(crate) fn held_key_is_member(
    owner_vk: &ed25519_dalek::VerifyingKey,
    members: &[river_core::room_state::member::AuthorizedMember],
    self_vk: &ed25519_dalek::VerifyingKey,
) -> bool {
    self_vk == owner_vk || members.iter().any(|m| &m.member.member_vk == self_vk)
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
        // The invitee is already in `members`. If they are also missing
        // from `member_info` (a retry of a partially-completed accept, or
        // a legacy stranding) this PUT does not heal them — but the room
        // is now an existing room, so the GET-path `build_member_info_heal`
        // self-heal covers it on the next GET. Same delayed-heal class as
        // issue #295.
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
    use river_core::room_state::privacy::SealedBytes;

    /// Build a public-room `AuthorizedMemberInfo` self-signed by `sk`,
    /// so the `build_state_for_put` tests can supply the member_info
    /// parameter the production caller builds before the PUT.
    fn test_member_info(sk: &SigningKey) -> AuthorizedMemberInfo {
        AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: MemberId::from(&sk.verifying_key()),
                version: 0,
                preferred_nickname: SealedBytes::public(b"Tester".to_vec()),
                deputies: Vec::new(),
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

    /// `build_state_for_put` with `member_info: None` — the private-room
    /// path where the room secret was unavailable to seal the nickname.
    /// The member + join_event are still injected, but NO `member_info`
    /// entry is added; the self-heal restores it (properly sealed) on a
    /// later GET. A regression that re-added an unconditional
    /// `member_info` push — the PR #272 mistake — would fail this test.
    #[test]
    fn build_state_for_put_omits_member_info_when_none() {
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

        let (put_state, join) = build_state_for_put(
            state,
            owner_vk,
            &invitee_sk,
            &authorized_member,
            None,
            fixed_time,
        )
        .expect("non-capacity invitee path must succeed");

        let invitee_id = MemberId::from(&invitee_vk);
        assert!(
            put_state
                .members
                .members
                .iter()
                .any(|m| m.member.member_vk == invitee_vk),
            "member is still injected even when member_info is None"
        );
        assert!(join.is_some(), "join_event is still synthesised");
        assert!(
            !put_state
                .member_info
                .member_info
                .iter()
                .any(|i| i.member_info.member_id == invitee_id),
            "member_info must be omitted when None is passed"
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

    /// freenet/river#292 Task 3/4 — the backward-probe trigger gate.
    ///
    /// `handle_get_response` fires the backward probe when a GET for the
    /// room's CURRENT contract key comes back with no real state. "No
    /// real state" is defined exactly as
    /// `configuration.verify_signature(&owner_vk).is_err()` — the same
    /// predicate as `RoomData::is_awaiting_initial_sync`.
    ///
    /// This test pins the two ends of that predicate:
    /// * a default `ChatRoomStateV1` — what an empty contract returns,
    ///   and what a freshly-imported (Task 4) or migrated room carries
    ///   before its first sync — MUST be classified as "no real state",
    ///   so the probe fires;
    /// * a genuine owner-signed configuration MUST be classified as
    ///   "real state", so the probe does NOT fire and the authoritative
    ///   current-key state is adopted instead.
    ///
    /// If this gate ever inverted, an imported room several generations
    /// behind would either never recover (probe never fires) or would
    /// clobber good current-key state with a needless probe.
    #[test]
    fn backward_probe_gate_classifies_real_vs_empty_state() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();

        // Empty / default state — an empty contract, or a fresh import
        // before sync. Its configuration is signed by the all-zero key.
        let empty = ChatRoomStateV1::default();
        assert!(
            empty.configuration.verify_signature(&owner_vk).is_err(),
            "default ChatRoomStateV1 must NOT verify against a real owner — \
             this is what makes the backward probe fire for a stranded import"
        );

        // Genuine owner-signed configuration — the probe must NOT fire.
        let real = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk),
            ..Default::default()
        };
        assert!(
            real.configuration.verify_signature(&owner_vk).is_ok(),
            "an owner-signed configuration must verify — the current key is \
             authoritative and the probe must not run"
        );
    }

    /// freenet/river#292 Task 3/4 — source-grep pin: a freshly imported
    /// room flows through `ImportIdentityModal` into `ROOMS` with default
    /// state, then through `process_rooms` (GET-first because
    /// `is_awaiting_initial_sync`) and lands in the existing-room branch
    /// of `handle_get_response`. That branch MUST route through
    /// `start_backward_probe` so an import several generations behind
    /// recovers real state instead of spinning or showing stale IDs.
    ///
    /// A future refactor that drops the `start_backward_probe` call would
    /// silently re-regress #292 for the import path; this pin fails CI if
    /// it does.
    #[test]
    fn import_recovery_routes_through_backward_probe() {
        // Search only the PRODUCTION half of the file — slice off `mod tests`
        // so this pin can't be satisfied by its own assertion-message text
        // (the failure mode the response_handler.rs pins also guard against).
        let full = include_str!("get_response.rs");
        let src = full
            .split("mod tests {")
            .next()
            .expect("get_response.rs must have production code before `mod tests`");
        assert!(
            src.contains("start_backward_probe(owner_vk,"),
            "handle_get_response must call start_backward_probe for a current-key \
             GET that returned no real state — this is the recovery entry point \
             for imported / migrated rooms stranded under an older room-contract \
             generation (freenet/river#292)."
        );
        assert!(
            src.contains("is_probe_instance(key.id())"),
            "handle_get_response must route legacy-key GET responses into the \
             probe handler via is_probe_instance (freenet/river#292)."
        );
    }

    /// freenet/river#414 round-10 P2 — source-grep pin: the post-GET migration
    /// completion callback must guard `key_migrated_to_delegate = true` +
    /// `remove_unverifiable_messages` on the captured key still matching the
    /// room's CURRENT `self_sk`, exactly like the other three migration
    /// callbacks. Without it, an identity overwrite mid-migration marks the NEW
    /// RoomData migrated (and sanitizes it) for a superseded key.
    #[test]
    fn post_get_migration_callback_guards_on_current_self_sk() {
        let full = include_str!("get_response.rs");
        let src = full
            .split("mod tests {")
            .next()
            .expect("get_response.rs must have production code before `mod tests`");
        assert!(
            src.contains("if room_data.self_sk != migrated_key {"),
            "the post-GET migration completion callback must discard a stale-key \
             result (room_data.self_sk != migrated_key) before marking migrated \
             (freenet/river#414 round-10 P2)."
        );
    }

    /// freenet/river#365 — source-grep pin: the invitation-accept GET handler
    /// must NOT overwrite an existing per-room `self_sk` that is already a valid
    /// member. Doing so orphans the original membership — the user keeps a
    /// duplicate member entry they can never remove. The accept-time guard in
    /// `receive_invitation_modal::accept_invitation` blocks the user-facing
    /// path, but this reassignment is the structural mechanism, so it must stay
    /// gated on a membership check. A refactor that re-introduces the old
    /// unconditional owner-only overwrite would re-regress #365 for any future
    /// caller; this pin fails CI if it does.
    #[test]
    fn self_sk_overwrite_is_gated_on_existing_membership() {
        // Search only the PRODUCTION half of the file — slice off `mod tests`
        // so this pin can't be satisfied by its own assertion-message text.
        let full = include_str!("get_response.rs");
        let src = full
            .split("mod tests {")
            .next()
            .expect("get_response.rs must have production code before `mod tests`");
        assert!(
            src.contains("if !existing_is_member {"),
            "the existing-room self_sk reassignment must be gated on \
             `!existing_is_member` so it never clobbers a live membership \
             (freenet/river#365)."
        );
        assert!(
            !src.contains("if room_data.self_sk.verifying_key() != owner_vk {"),
            "the old owner-only self_sk overwrite condition must be gone — it let \
             a re-accept orphan an existing membership (freenet/river#365)."
        );
    }

    /// freenet/river#292 — `merge_room_states` CRDT-merges a recovered legacy
    /// state with the device's local snapshot before the migrating PUT. It
    /// must preserve the recovered state's owner-signed configuration (so the
    /// merged state remains valid) and must not panic when the local snapshot
    /// is an empty default.
    #[test]
    fn merge_room_states_preserves_recovered_configuration() {
        let owner_sk = SigningKey::from_bytes(&[11u8; 32]);
        let owner_vk = owner_sk.verifying_key();
        let params = ChatRoomParametersV1 { owner: owner_vk };

        // A recovered legacy state with a real owner-signed configuration.
        let recovered = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(Configuration::default(), &owner_sk),
            ..Default::default()
        };

        // Merging in an empty/default local snapshot (the fresh-import case)
        // must keep the recovered state usable — its signed configuration
        // survives the merge.
        let merged = merge_room_states(recovered, &ChatRoomStateV1::default(), &params);
        assert!(
            merged.configuration.verify_signature(&owner_vk).is_ok(),
            "merge_room_states must preserve the recovered state's owner-signed configuration"
        );
    }

    /// freenet/river#367 — the `held_key_is_member` predicate that gates the
    /// re-accept short-circuit. It must report "already a member" for:
    /// * the owner (always a member of their own room), and
    /// * a held `self_vk` that is in the member list,
    /// and must NOT report a fresh, non-member key (the case a genuine new
    /// join hits). This is the same shape as the accept-time guard's
    /// `vk_is_room_member`; if the two ever diverge, the structural backstop
    /// would stop matching the rooms the entry-level guard catches.
    #[test]
    fn held_key_is_member_matches_owner_and_members() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let owner_vk = owner_sk.verifying_key();
        let member_sk = SigningKey::generate(&mut rng);
        let member_vk = member_sk.verifying_key();
        let stranger_vk = SigningKey::generate(&mut rng).verifying_key();

        let members = vec![AuthorizedMember::new(
            Member {
                owner_member_id: owner_vk.into(),
                invited_by: owner_vk.into(),
                member_vk,
            },
            &owner_sk,
        )];

        // Owner is always a member of their own room.
        assert!(
            held_key_is_member(&owner_vk, &members, &owner_vk),
            "owner's held key must be reported as a member"
        );
        // A held key present in the member list is a member.
        assert!(
            held_key_is_member(&owner_vk, &members, &member_vk),
            "a held self_vk in the member list must be reported as a member"
        );
        // A fresh / unrelated key (the genuine-new-join case) is NOT.
        assert!(
            !held_key_is_member(&owner_vk, &members, &stranger_vk),
            "a key that is neither owner nor in the member list must NOT be reported \
             as a member — otherwise a genuine first join would be wrongly short-circuited"
        );
    }

    /// freenet/river#367 — source-grep pin: the invitation-accept GET handler
    /// MUST short-circuit to a no-op refresh, BEFORE `build_state_for_put`,
    /// when the user is already a member under their held `self_sk`. The full
    /// handler reads the `ROOMS`/`PENDING_INVITES` signals and PUTs over the
    /// WebApi, so it can't be exercised by a host unit test; this pin locks in
    /// the structural backstop instead.
    ///
    /// A refactor that removed the short-circuit — or moved it AFTER
    /// `build_state_for_put` — would re-regress #365 for any path that reaches
    /// this handler with `PENDING_INVITES` populated for an already-member
    /// room (e.g. the accept-time guard falling through on an unreadable
    /// `ROOMS`). This pin fails CI if it does.
    #[test]
    fn reaccept_get_short_circuits_before_build_state_for_put() {
        // Search only the PRODUCTION half of the file — slice off `mod tests`
        // so this pin can't be satisfied by its own assertion-message text.
        let full = include_str!("get_response.rs");
        let src = full
            .split("mod tests {")
            .next()
            .expect("get_response.rs must have production code before `mod tests`");

        let guard_idx = src.find("let already_member_under_held_key").expect(
            "handle_get_response must compute `already_member_under_held_key` from the \
                 held self_sk to short-circuit a re-accept (freenet/river#367).",
        );
        let build_idx = src
            .find("build_state_for_put(")
            .expect("handle_get_response must still call build_state_for_put on the new-join path");
        assert!(
            guard_idx < build_idx,
            "the re-accept short-circuit must be evaluated BEFORE build_state_for_put — \
             otherwise a duplicate member is PUT before the no-op refresh can intervene \
             (freenet/river#367)."
        );
        assert!(
            src.contains("held_key_is_member("),
            "the short-circuit must gate on `held_key_is_member` so it agrees with the \
             accept-time guard's membership predicate (freenet/river#367)."
        );
        // The membership decision must be made against the freshly-fetched
        // CANONICAL state (`retrieved_state`), not the local ROOMS snapshot —
        // otherwise a stale local snapshot (member pruned/banned remotely)
        // would wrongly suppress a legitimate rejoin. (Codex review of this
        // PR.) Pin that the predicate is applied to retrieved_state.members.
        assert!(
            src.contains("held_key_is_member(&owner_vk, &retrieved_state.members.members,"),
            "the re-accept short-circuit must judge membership against the canonical \
             `retrieved_state`, not the local ROOMS snapshot, so a stale local \
             membership can't suppress a legitimate rejoin (freenet/river#367)."
        );
    }
}
