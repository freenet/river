use crate::components::app::freenet_api::constants::REPUT_DELAY_MS;
use crate::components::app::freenet_api::error_report::{ErrorReport, LABEL_ROOM_NOT_FOUND};
use crate::components::app::ROOMS;
use crate::util::owner_vk_to_contract_key;
use dioxus::logger::tracing::{debug, warn};
use dioxus::prelude::{Global, GlobalSignal, ReadableExt};
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::tracing::info;
use freenet_stdlib::prelude::ContractInstanceId;
use river_core::room_state::member::MemberId;
use river_core::ChatRoomStateV1;
use std::collections::{HashMap, HashSet};

/// Get current time in milliseconds (works in WASM)
pub(crate) fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0)
    }
}

pub static SYNC_INFO: GlobalSignal<SyncInfo> = Global::new(SyncInfo::new);

/// How many times a room's initial sync may fail before it is promoted to a
/// terminal [`RoomSyncStatus::Error`] instead of being retried forever
/// (freenet/river#290).
///
/// A restored/imported room whose contract is genuinely absent from the
/// network never settles: each GET fails, the room is reset to
/// `Disconnected` (or its `Subscribing` times out), and the next
/// `ProcessRooms` cycle re-GETs it — spinning the "Syncing room state…"
/// indicator forever. Bounding the retries lets the UI surface a terminal
/// error instead.
///
/// The bound is deliberately generous so it does NOT regress the
/// freenet-core#1470 contract-creation race (a freshly-PUT contract that is
/// briefly "not found" while creation completes), which resolves within a
/// couple of retries.
pub const MAX_SYNC_ATTEMPTS_BEFORE_ERROR: u32 = 10;

/// Message shown (and stored in [`RoomSyncStatus::Error`]) when a room's
/// contract could not be found on the network after the retry bound.
pub const CONTRACT_NOT_FOUND_ERROR: &str =
    "This room could not be found on the network — it may have been removed.";

pub struct SyncInfo {
    map: HashMap<VerifyingKey, RoomSyncInfo>,
    instances: HashMap<ContractInstanceId, VerifyingKey>,
    /// Rooms whose contract we have reason to believe the network already
    /// holds: loaded from delegate storage, or subscribed earlier this session.
    /// Only these subscribe with a code-free GET; everything else (a room
    /// created this session, say) seeds with a full PUT as before
    /// (freenet/river#757). Kept outside `map` so it can be set before the room
    /// is registered, which must not happen early: registration in
    /// `rooms_awaiting_subscription` is what seeds `last_synced_state`.
    known_on_network: HashSet<VerifyingKey>,
}

/// How [`crate::components::app::freenet_api::room_synchronizer::RoomSynchronizer::process_rooms`]
/// subscribes a room that already holds valid state (freenet/river#757).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubscribeRoute {
    /// `GET { return_contract_code: false, subscribe: true }`: no upload. The
    /// node fetches and caches the contract code itself, registers this
    /// client's subscription, and returns the network's state, which is merged
    /// locally and pushes back any local-only changes as a delta UPDATE.
    Get,
    /// `PUT { contract, state, subscribe: true }`: uploads the contract WASM and
    /// the full local state. This is the re-seed that recovers a room the
    /// network lost (a77bfe76), and the only way to create a new room.
    Put,
}

pub struct RoomSyncInfo {
    pub sync_status: RoomSyncStatus,
    // TODO: Would be better if state implemented Hash trait and just store
    //       a hash of the state
    pub last_synced_state: Option<ChatRoomStateV1>,
    /// Timestamp (in ms) when subscription was initiated, used for timeout detection
    pub subscribing_since: Option<f64>,
    /// How many sync attempts have failed for this room. Reset to 0 once the
    /// room reaches [`RoomSyncStatus::Subscribed`]. Used to bound the
    /// otherwise-infinite retry loop for a room STILL awaiting its initial sync
    /// whose contract is absent from the network (freenet/river#290); for a
    /// room that already holds valid synced state the counter is tracked but
    /// never trips the bound (see `record_failed_sync_attempt`).
    pub failed_sync_attempts: u32,
    /// A code-free subscribe GET ([`SubscribeRoute::Get`]) is outstanding for
    /// this room (freenet/river#757).
    pub subscribe_get_in_flight: bool,
    /// The last subscribe GET did not come back with state — the node answered
    /// `NotFound`, an error, or nothing in time — so the next attempt must
    /// re-seed with a full PUT. Cleared once the room reaches `Subscribed`.
    pub needs_seed_put: bool,
    /// A sync pass skipped this room's unsent local change because the room
    /// was not yet `Subscribed`. Nothing else re-runs the pass when the
    /// status flips, so [`SyncInfo::mark_subscribed`] reports it and
    /// [`mark_room_subscribed`] requests one. Without it a message posted
    /// between an invitation's PUT and its PutResponse waited for an
    /// unrelated later pass, which a fresh profile (no rooms loaded, so no
    /// post-load check) never got.
    ///
    /// Cleared only when the change is sent ([`SyncInfo::state_updated`]) or
    /// a pass on the subscribed room finds nothing to send, never when a pass
    /// is merely requested: a requested pass can do nothing (`ROOMS`
    /// unreadable, no web API), and the next `Subscribed` report must ask
    /// again. An extra request costs nothing, since a pass compares against
    /// the last synced state.
    pub update_held: bool,
}

impl SyncInfo {
    pub fn new() -> Self {
        SyncInfo {
            map: HashMap::new(),
            instances: HashMap::new(),
            known_on_network: HashSet::new(),
        }
    }

    pub fn register_new_room(&mut self, owner_key: VerifyingKey) {
        let contract_key = owner_vk_to_contract_key(&owner_key);
        let contract_id = contract_key.id();

        if let std::collections::hash_map::Entry::Vacant(e) = self.map.entry(owner_key) {
            debug!(
                "Registering new room with owner key: {:?}, contract ID: {}",
                MemberId::from(owner_key),
                contract_id
            );

            e.insert(RoomSyncInfo {
                sync_status: RoomSyncStatus::Disconnected,
                last_synced_state: None,
                subscribing_since: None,
                failed_sync_attempts: 0,
                subscribe_get_in_flight: false,
                needs_seed_put: false,
                update_held: false,
            });

            self.instances.insert(*contract_id, owner_key);
            debug!(
                "Added mapping from contract ID {} to owner key {:?}",
                contract_id,
                MemberId::from(owner_key)
            );
        } else {
            debug!(
                "Room with owner key {:?} already registered",
                MemberId::from(owner_key)
            );
        }
    }

    pub fn update_sync_status(&mut self, owner_key: &VerifyingKey, status: RoomSyncStatus) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            // Track when subscription starts for timeout detection
            if status == RoomSyncStatus::Subscribing {
                sync_info.subscribing_since = Some(now_ms());
            } else {
                sync_info.subscribing_since = None;
            }
            // A successful subscription clears the failed-attempt budget so a
            // later transient outage doesn't inherit a near-exhausted counter
            // and give up prematurely (freenet/river#290).
            if status == RoomSyncStatus::Subscribed {
                sync_info.failed_sync_attempts = 0;
                // Subscribed means the network holds the contract now, however
                // we got here, so a later re-subscribe (a reconnect) can skip
                // the upload (freenet/river#757).
                sync_info.subscribe_get_in_flight = false;
                sync_info.needs_seed_put = false;
                self.known_on_network.insert(*owner_key);
            }
            sync_info.sync_status = status;
        }
    }

    /// Mark the room `Subscribed`. Returns whether a local change skipped
    /// while it was not subscribed is still unsent, in which case the caller
    /// must request a pass ([`mark_room_subscribed`] does). Does not clear the
    /// record; see [`RoomSyncInfo::update_held`].
    pub fn mark_subscribed(&mut self, owner_key: &VerifyingKey) -> bool {
        self.update_sync_status(owner_key, RoomSyncStatus::Subscribed);
        self.map.get(owner_key).is_some_and(|info| info.update_held)
    }

    /// Whether `room_state` must be sent for `key` in this pass: the room is
    /// `Subscribed` and its state differs from what was last synced. A change
    /// skipped because the room is not subscribed yet is recorded for
    /// [`SyncInfo::mark_subscribed`]. Registers an unknown room.
    pub fn room_needs_update(&mut self, key: &VerifyingKey, room_state: &ChatRoomStateV1) -> bool {
        if !self.map.contains_key(key) {
            info!("Registering new room: {:?}", key);
            self.register_new_room(*key);
        }
        let Some(sync_info) = self.map.get_mut(key) else {
            return false;
        };
        let states_match = sync_info.last_synced_state.as_ref() == Some(room_state);
        if sync_info.sync_status == RoomSyncStatus::Subscribed {
            if states_match {
                // Nothing left to send, held or not.
                sync_info.update_held = false;
                debug!(
                    "Room {:?} doesn't need update - state unchanged",
                    MemberId::from(*key)
                );
            }
            !states_match
        } else {
            if !states_match {
                sync_info.update_held = true;
            }
            debug!(
                "Room {:?} doesn't need update - not subscribed (status: {:?})",
                MemberId::from(*key),
                sync_info.sync_status
            );
            false
        }
    }

    /// Record that `owner_key`'s room came out of delegate storage, so its
    /// contract has (very likely) been on the network before and the first
    /// subscribe can try a code-free GET (freenet/river#757). If the network
    /// has since lost it, the GET's failure falls back to the re-seeding PUT.
    pub fn mark_known_on_network(&mut self, owner_key: VerifyingKey) {
        self.known_on_network.insert(owner_key);
    }

    /// Which way to subscribe `owner_key`'s room next (freenet/river#757). The
    /// GET is tried only for a room believed to be on the network whose last
    /// subscribe GET did not fail; anything else seeds with a PUT, which is
    /// what every subscribe did before this route existed.
    pub fn subscribe_route(&self, owner_key: &VerifyingKey) -> SubscribeRoute {
        let failed_get = self
            .map
            .get(owner_key)
            .is_some_and(|info| info.needs_seed_put);
        if self.known_on_network.contains(owner_key) && !failed_get {
            SubscribeRoute::Get
        } else {
            SubscribeRoute::Put
        }
    }

    /// The node refused a subscription for `owner_key` with local state on
    /// hand: re-seed it with the PUT next, whichever route the refused
    /// subscription came from (freenet/river#757). Before the GET route every
    /// subscription followed a PUT, so this was already the behaviour.
    pub fn require_seed_put(&mut self, owner_key: &VerifyingKey) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            sync_info.subscribe_get_in_flight = false;
            sync_info.needs_seed_put = true;
        }
    }

    /// A subscribe GET was sent for `owner_key` (freenet/river#757).
    pub fn note_subscribe_get_sent(&mut self, owner_key: &VerifyingKey) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            sync_info.subscribe_get_in_flight = true;
        }
    }

    /// A `GetResponse` with state arrived for `owner_key`. Returns `true` if it
    /// answers an outstanding subscribe GET, in which case the caller completes
    /// the subscription from it (freenet/river#757).
    pub fn take_subscribe_get(&mut self, owner_key: &VerifyingKey) -> bool {
        match self.map.get_mut(owner_key) {
            Some(sync_info) if sync_info.subscribe_get_in_flight => {
                sync_info.subscribe_get_in_flight = false;
                true
            }
            _ => false,
        }
    }

    /// The node reported `owner_key`'s contract missing (or returned no usable
    /// state for it). Returns `true` if that answers an outstanding subscribe
    /// GET: the room must now be re-seeded with a PUT, and the caller does so
    /// at once (freenet/river#757). Any later attempt also PUTs until the room
    /// reaches `Subscribed`.
    pub fn subscribe_get_found_nothing(&mut self, owner_key: &VerifyingKey) -> bool {
        match self.map.get_mut(owner_key) {
            Some(sync_info) if sync_info.subscribe_get_in_flight => {
                sync_info.subscribe_get_in_flight = false;
                sync_info.needs_seed_put = true;
                true
            }
            _ => false,
        }
    }

    /// Mark every room this page believes is `Subscribed` as `Disconnected`, so
    /// the existing `process_rooms()` path re-subscribes it. Returns how many
    /// rooms were flipped (for logging).
    ///
    /// **Call this on a genuinely NEW WebSocket only.** A client subscription
    /// lives on the node against a `ClientId`, and the node tears down every
    /// subscription for that client on each socket exit path
    /// (`websocket.rs::notify_disconnect` -> `remove_client_from_all_subscriptions`).
    /// A reconnect gets a fresh `ClientId`, so after it the node holds NO client
    /// subscription for any room — while `SYNC_INFO` is a page-lifetime signal
    /// that still says `Subscribed`. Since `rooms_awaiting_subscription` only
    /// returns `Disconnected` (or timed-out `Subscribing`) rooms, nothing ever
    /// re-subscribed: the tab reconnected, showed "Connected", and then received
    /// no further updates for the rest of the session. A reload cured it only
    /// because it started with an empty `SYNC_INFO`.
    ///
    /// Deliberately NOT called from the `PageBecameVisible` / `RefreshAllRooms`
    /// path: that path runs on an apparently-live socket, where the node's client
    /// subscriptions are still intact, and re-subscribing there would ship full
    /// room state for every room on every tab focus.
    ///
    /// Only `Subscribed` is flipped. `Subscribing` is left alone so its timeout
    /// bookkeeping is not disturbed, and terminal `Error` is left alone so a room
    /// that has exhausted its #290 retry budget is not silently revived. Flipping
    /// to `Disconnected` costs no retry budget: `update_sync_status` only clears
    /// `failed_sync_attempts` on `Subscribed`.
    pub fn mark_subscribed_rooms_disconnected(&mut self) -> usize {
        let stale: Vec<VerifyingKey> = self
            .map
            .iter()
            .filter(|(_, info)| info.sync_status == RoomSyncStatus::Subscribed)
            .map(|(key, _)| *key)
            .collect();
        for key in &stale {
            self.update_sync_status(key, RoomSyncStatus::Disconnected);
        }
        stale.len()
    }

    /// Record a failed sync attempt for a room and decide whether to keep
    /// retrying or give up (freenet/river#290).
    ///
    /// Increments the room's `failed_sync_attempts` and always clears
    /// `subscribing_since`. The retry bound applies **only** to a room that is
    /// still awaiting its initial sync (`awaiting_initial_sync == true`) — i.e.
    /// a restored/imported room holding placeholder state whose contract may be
    /// absent from the network. For such a room, once the count reaches
    /// [`MAX_SYNC_ATTEMPTS_BEFORE_ERROR`] it is promoted to the terminal
    /// [`RoomSyncStatus::Error`] and `false` is returned (stop retrying);
    /// before that it is reset to [`RoomSyncStatus::Disconnected`] to retry.
    ///
    /// A room that already holds **valid synced state**
    /// (`awaiting_initial_sync == false`) is NEVER given up on: a transient
    /// outage must not strand a working room. It is always reset to
    /// `Disconnected` so the existing self-healing re-PUT behaviour continues
    /// indefinitely. (The counter is still incremented so logs reflect the
    /// failure streak, but it never trips the bound for these rooms.)
    ///
    /// Returns `true` if the caller should schedule a retry, `false` if the
    /// room has been given up on. A room not present in the map (already
    /// removed) returns `false`.
    pub fn record_failed_sync_attempt(
        &mut self,
        owner_key: &VerifyingKey,
        awaiting_initial_sync: bool,
    ) -> bool {
        let Some(sync_info) = self.map.get_mut(owner_key) else {
            return false;
        };
        sync_info.failed_sync_attempts = sync_info.failed_sync_attempts.saturating_add(1);
        sync_info.subscribing_since = None;
        // A subscribe GET that timed out or errored proves nothing about
        // whether the network has the room, so retry the way that cannot miss:
        // the re-seeding PUT (freenet/river#757).
        if sync_info.subscribe_get_in_flight {
            sync_info.subscribe_get_in_flight = false;
            sync_info.needs_seed_put = true;
        }
        if awaiting_initial_sync && sync_info.failed_sync_attempts >= MAX_SYNC_ATTEMPTS_BEFORE_ERROR
        {
            warn!(
                "Room {:?} failed initial sync {} times — giving up (contract absent from network)",
                MemberId::from(*owner_key),
                sync_info.failed_sync_attempts
            );
            sync_info.sync_status = RoomSyncStatus::Error(ErrorReport::new(
                LABEL_ROOM_NOT_FOUND,
                CONTRACT_NOT_FOUND_ERROR,
                format!(
                    "Initial sync failed {} times; the room's contract was not found on the network",
                    sync_info.failed_sync_attempts
                ),
            ));
            false
        } else {
            sync_info.sync_status = RoomSyncStatus::Disconnected;
            true
        }
    }

    /// Refresh `subscribing_since` for a room that is still `Subscribing`,
    /// without otherwise changing its status. A backward-probe recovery
    /// (freenet/river#292) can run longer than `REPUT_DELAY_MS`; calling this
    /// each probe hop keeps `rooms_awaiting_subscription` from reclaiming the
    /// room mid-probe. Unlike `update_sync_status(.., Subscribing)` this does
    /// NOT force the status, so a room that genuinely reached `Subscribed`
    /// mid-probe is left alone (and is not swept anyway).
    pub fn touch_subscribing_since(&mut self, owner_key: &VerifyingKey) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            if sync_info.sync_status == RoomSyncStatus::Subscribing {
                sync_info.subscribing_since = Some(now_ms());
            }
        }
    }

    pub fn update_last_synced_state(&mut self, owner_key: &VerifyingKey, state: &ChatRoomStateV1) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            sync_info.last_synced_state = Some(state.clone());
        }
    }

    pub fn get_sync_status(&self, owner_key: &VerifyingKey) -> Option<&RoomSyncStatus> {
        self.map.get(owner_key).map(|info| &info.sync_status)
    }

    /// Connection-pill activity only counts registered, live rooms. A room
    /// with previously synced state keeps retrying after the initial-sync
    /// bound, but its activity indicator can still settle.
    pub(crate) fn has_syncing_rooms(&self, live_keys: &[VerifyingKey]) -> bool {
        live_keys.iter().any(|k| {
            self.map.get(k).is_some_and(|info| {
                matches!(
                    info.sync_status,
                    RoomSyncStatus::Disconnected | RoomSyncStatus::Subscribing
                ) && info.failed_sync_attempts < MAX_SYNC_ATTEMPTS_BEFORE_ERROR
            })
        })
    }

    pub fn get_owner_vk_for_instance_id(
        &self,
        instance_id: &ContractInstanceId,
    ) -> Option<VerifyingKey> {
        let result = self.instances.get(instance_id).copied();
        if result.is_some() {
            debug!("Found owner key for contract ID {}", instance_id);
        } else {
            debug!("No owner key found for contract ID {}", instance_id);
            // Log all known mappings to help debug
            for (id, vk) in &self.instances {
                debug!(
                    "Known mapping: contract ID {} -> owner key {:?}",
                    id,
                    MemberId::from(*vk)
                );
            }
        }
        result
    }

    pub fn rooms_awaiting_subscription(&mut self) -> HashMap<VerifyingKey, ChatRoomStateV1> {
        let mut rooms_awaiting_subscription = HashMap::new();
        // Use try_read() to avoid panic when ROOMS is mutably borrowed.
        // This can happen because Dioxus's write guard Drop notifies subscribers
        // synchronously, which can trigger process_rooms() while the write guard
        // is still being dropped. If ROOMS is borrowed, we skip this cycle —
        // we'll be called again on the next ProcessRooms.
        let Ok(rooms) = ROOMS.try_read() else {
            debug!("ROOMS is currently borrowed, skipping rooms_awaiting_subscription");
            return rooms_awaiting_subscription;
        };
        let current_time = now_ms();

        // Rooms whose `Subscribing` status has timed out. Collected here
        // because we can't mutate `self.map` while iterating it below; the
        // timeout counts as a failed sync attempt (freenet/river#290).
        let mut timed_out: Vec<VerifyingKey> = Vec::new();

        for (key, room_data) in rooms.map.iter() {
            // Register new rooms automatically
            if !self.map.contains_key(key) {
                self.register_new_room(*key);
                self.update_last_synced_state(key, &room_data.room_state);
            }

            let sync_info = self.map.get(key).unwrap();

            // Add room to awaiting list if it's disconnected
            if sync_info.sync_status == RoomSyncStatus::Disconnected {
                rooms_awaiting_subscription.insert(*key, room_data.room_state.clone());
            }

            // Check for subscription timeout - if subscribing for longer than REPUT_DELAY_MS,
            // count a failed attempt and (unless the retry bound is reached) re-PUT.
            if sync_info.sync_status == RoomSyncStatus::Subscribing {
                if let Some(started_at) = sync_info.subscribing_since {
                    let elapsed_ms = current_time - started_at;
                    if elapsed_ms >= REPUT_DELAY_MS as f64 {
                        warn!(
                            "Subscription timeout for room {:?} after {:.1}s - will retry with a seeding PUT",
                            MemberId::from(*key),
                            elapsed_ms / 1000.0
                        );
                        timed_out.push(*key);
                    }
                }
            }
        }

        // Now record the failure for timed-out rooms. `record_failed_sync_attempt`
        // resets the room to `Disconnected` (retry) or, once the bound is hit
        // for a room STILL awaiting its initial sync, promotes it to terminal
        // `Error` — in which case it must NOT be re-added to the awaiting set,
        // so the spinner can stop (freenet/river#290). A room that already holds
        // valid synced state is never given up on; it keeps retrying.
        for key in timed_out {
            let awaiting_initial_sync = rooms
                .map
                .get(&key)
                .is_some_and(|rd| rd.is_awaiting_initial_sync());
            let should_retry = self.record_failed_sync_attempt(&key, awaiting_initial_sync);
            if should_retry {
                if let Some(room_data) = rooms.map.get(&key) {
                    rooms_awaiting_subscription.insert(key, room_data.room_state.clone());
                }
            }
        }

        rooms_awaiting_subscription
    }

    /// Returns rooms needing sync: current state + last synced baseline (if any).
    /// The baseline is used by the caller to compute a delta instead of sending full state.
    pub fn needs_to_send_update(
        &mut self,
    ) -> HashMap<VerifyingKey, (ChatRoomStateV1, Option<ChatRoomStateV1>)> {
        let mut rooms_needing_update = HashMap::new();

        // FIXME: Temporarily disabled to fix infinite loop bug
        // This secret rotation/generation code was modifying ROOMS inside a "check if sync needed" function,
        // which triggered use_effect → ProcessRooms → needs_to_send_update → ROOMS.with_mut → use_effect (infinite loop)
        //
        // TODO: Move this logic to a separate periodic task that runs independently of sync triggers
        // See: https://github.com/freenet/river/issues/XXX
        //
        // let keys_to_process: Vec<VerifyingKey> = ROOMS.read().map.keys().copied().collect();
        //
        // for key in &keys_to_process {
        //     let should_generate = ROOMS.read().map.get(key).map(|room_data| {
        //         room_data.owner_vk == room_data.self_sk.verifying_key()
        //     }).unwrap_or(false);
        //
        //     if should_generate {
        //         ROOMS.with_mut(|rooms| {
        //             if let Some(room_data) = rooms.map.get_mut(key) {
        //                 if room_data.needs_secret_rotation() { ... }
        //                 if let Some(secrets_delta) = room_data.generate_missing_member_secrets() { ... }
        //             }
        //         });
        //     }
        // }

        // Second pass: check which rooms need updates
        let Ok(rooms) = ROOMS.try_read() else {
            debug!("ROOMS is currently borrowed, skipping needs_to_send_update");
            return rooms_needing_update;
        };

        debug!(
            "Checking for rooms that need updates, total rooms: {}",
            rooms.map.len()
        );

        for (key, room_data) in rooms.map.iter() {
            if self.room_needs_update(key, &room_data.room_state) {
                info!(
                    "Room {:?} needs update - state has changed",
                    MemberId::from(key)
                );
                // The last synced state is updated after a successful send.
                let last_synced = self
                    .map
                    .get(key)
                    .and_then(|info| info.last_synced_state.clone());
                rooms_needing_update.insert(*key, (room_data.room_state.clone(), last_synced));
            }
        }

        info!("Found {} rooms needing updates", rooms_needing_update.len());
        rooms_needing_update
    }

    /// Register that the state's current value has been sent to the network
    pub fn state_updated(&mut self, owner_key: &VerifyingKey, new_state: ChatRoomStateV1) {
        if let Some(sync_info) = self.map.get_mut(owner_key) {
            sync_info.last_synced_state = Some(new_state);
            // The held change, if any, has been sent.
            sync_info.update_held = false;
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum RoomSyncStatus {
    Disconnected,

    Subscribing,

    Subscribed,

    Error(ErrorReport),
}

/// Mark `owner_vk`'s room `Subscribed`, and request a sync pass if one skipped
/// a local change while the room was still subscribing (a message posted
/// between an invitation's PUT and its PutResponse). Call from a deferred
/// context. The pass sends nothing if the change already went out:
/// `needs_to_send_update` compares against the last synced state.
pub(crate) fn mark_room_subscribed(owner_vk: VerifyingKey) {
    let mut requested = None;
    SYNC_INFO.with_mut(|sync_info| {
        subscribe_and_request_held_pass(sync_info, owner_vk, |vk| requested = Some(vk));
    });
    // Outside the SYNC_INFO borrow; `mark_needs_sync` defers its own write.
    if let Some(vk) = requested {
        crate::components::app::mark_needs_sync(vk);
    }
}

/// The decision behind [`mark_room_subscribed`], with the pass request passed
/// in so it is tested without the Dioxus runtime: mark the room `Subscribed`
/// and call `request_pass` once if a held change is still unsent.
pub(crate) fn subscribe_and_request_held_pass(
    sync_info: &mut SyncInfo,
    owner_vk: VerifyingKey,
    request_pass: impl FnOnce(VerifyingKey),
) {
    if sync_info.mark_subscribed(&owner_vk) {
        info!(
            "Room {:?} is subscribed with a local change held back; requesting a sync pass",
            MemberId::from(owner_vk)
        );
        request_pass(owner_vk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn pill_sync_activity_ignores_stale_unregistered_and_settled_rooms() {
        let mut info = SyncInfo::new();
        let live = test_owner(1);
        let stale = test_owner(2);
        info.register_new_room(stale);
        assert!(!info.has_syncing_rooms(&[live]));
        info.register_new_room(live);
        for (status, expected) in [
            (RoomSyncStatus::Disconnected, true),
            (RoomSyncStatus::Subscribing, true),
            (RoomSyncStatus::Subscribed, false),
            (
                RoomSyncStatus::Error(ErrorReport::new(LABEL_ROOM_NOT_FOUND, "gone", "gone")),
                false,
            ),
        ] {
            info.update_sync_status(&live, status);
            assert_eq!(info.has_syncing_rooms(&[live]), expected);
        }
    }

    #[test]
    fn pill_sync_activity_stops_at_the_bound_even_if_the_room_retries() {
        let mut info = SyncInfo::new();
        let live = test_owner(3);
        info.register_new_room(live);
        for _ in 1..MAX_SYNC_ATTEMPTS_BEFORE_ERROR {
            assert!(info.record_failed_sync_attempt(&live, false));
        }
        assert!(info.has_syncing_rooms(&[live]));
        assert!(info.record_failed_sync_attempt(&live, false));
        assert_eq!(
            info.get_sync_status(&live),
            Some(&RoomSyncStatus::Disconnected)
        );
        assert!(!info.has_syncing_rooms(&[live]));
    }

    /// A fresh profile's first message: posted while the joined room is still
    /// `Subscribing` (the invitation PUT is sent, its PutResponse is not back),
    /// the sync pass skips it. When the room becomes `Subscribed` a pass must
    /// be requested, it must send the change, and it must send it once.
    #[test]
    fn a_change_held_while_subscribing_is_sent_once_the_room_is_subscribed() {
        let mut si = SyncInfo::new();
        let room = test_owner(40);
        si.register_new_room(room);
        si.update_sync_status(&room, RoomSyncStatus::Subscribing);
        let joined = ChatRoomStateV1::default();
        si.update_last_synced_state(&room, &joined);

        let mut with_message = joined.clone();
        with_message.configuration.configuration.max_recent_messages += 1;

        // The pass the message triggers runs before the PutResponse.
        assert!(!si.room_needs_update(&room, &with_message));

        // PutResponse: the room is subscribed, and a pass is requested.
        assert!(
            si.mark_subscribed(&room),
            "a change held back while subscribing must request a sync pass"
        );
        assert_eq!(si.get_sync_status(&room), Some(&RoomSyncStatus::Subscribed));

        // That pass sends the change.
        assert!(si.room_needs_update(&room, &with_message));
        si.state_updated(&room, with_message.clone());

        // Exactly once: a later pass (the 21 s check, another trigger) and a
        // second Subscribed report send and request nothing.
        assert!(!si.room_needs_update(&room, &with_message));
        assert!(!si.mark_subscribed(&room));
    }

    /// A requested pass that does nothing (ROOMS unreadable, so
    /// `needs_to_send_update` returns early; or no web API, so the send never
    /// happens) must not consume the record: the next `Subscribed` report asks
    /// again, and the next real pass sends the change.
    #[test]
    fn a_held_change_survives_a_pass_that_sent_nothing() {
        let mut si = SyncInfo::new();
        let room = test_owner(43);
        si.register_new_room(room);
        si.update_sync_status(&room, RoomSyncStatus::Subscribing);
        let joined = ChatRoomStateV1::default();
        si.update_last_synced_state(&room, &joined);
        let mut with_message = joined.clone();
        with_message.configuration.configuration.max_recent_messages += 1;
        assert!(!si.room_needs_update(&room, &with_message));

        assert!(si.mark_subscribed(&room));
        // The requested pass found ROOMS unreadable: room_needs_update is not
        // called at all. The record stays, so a later report asks again.
        assert!(
            si.mark_subscribed(&room),
            "a degraded pass must not consume the record"
        );
        // A pass picks the room for sending, but the send never happens (no
        // web API): still held.
        assert!(si.room_needs_update(&room, &with_message));
        assert!(si.mark_subscribed(&room), "an unsent change must stay held");
        // The next pass sends it; only then is the record cleared.
        assert!(si.room_needs_update(&room, &with_message));
        si.state_updated(&room, with_message.clone());
        assert!(!si.mark_subscribed(&room));
        assert!(!si.room_needs_update(&room, &with_message));
    }

    /// The pass request itself, end to end through the function
    /// `mark_room_subscribed` delegates to: a held change requests exactly one
    /// pass per `Subscribed` report until it is sent, and nothing is requested
    /// without one.
    #[test]
    fn marking_subscribed_requests_a_pass_only_for_a_held_change() {
        let mut si = SyncInfo::new();
        let held = test_owner(44);
        let clean = test_owner(45);
        let joined = ChatRoomStateV1::default();
        let mut with_message = joined.clone();
        with_message.configuration.configuration.max_recent_messages += 1;
        for room in [held, clean] {
            si.register_new_room(room);
            si.update_sync_status(&room, RoomSyncStatus::Subscribing);
            si.update_last_synced_state(&room, &joined);
        }
        assert!(!si.room_needs_update(&held, &with_message));
        assert!(!si.room_needs_update(&clean, &joined));

        let mut requested = Vec::new();
        subscribe_and_request_held_pass(&mut si, held, |vk| requested.push(vk));
        subscribe_and_request_held_pass(&mut si, clean, |vk| requested.push(vk));
        assert_eq!(
            requested,
            vec![held],
            "only the room with a held change asks for a pass"
        );
        assert_eq!(
            si.get_sync_status(&clean),
            Some(&RoomSyncStatus::Subscribed)
        );

        // Sent: a second Subscribed report requests nothing.
        assert!(si.room_needs_update(&held, &with_message));
        si.state_updated(&held, with_message.clone());
        subscribe_and_request_held_pass(&mut si, held, |vk| requested.push(vk));
        assert_eq!(requested, vec![held]);
    }

    /// `mark_room_subscribed` must hand the decision's request to
    /// `mark_needs_sync`, or the request decided above never reaches the
    /// synchronizer.
    #[test]
    fn mark_room_subscribed_turns_the_request_into_a_sync_pass() {
        let src = include_str!("sync_info.rs");
        let prod = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let body: String = prod
            .split_once("pub(crate) fn mark_room_subscribed(owner_vk: VerifyingKey) {")
            .expect("mark_room_subscribed must exist")
            .1
            .split_once("\n}\n")
            .expect("mark_room_subscribed must end")
            .0
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            body.contains(
                "subscribe_and_request_held_pass(sync_info,owner_vk,|vk|requested=Some(vk));"
            ),
            "mark_room_subscribed must delegate the decision: {body}"
        );
        assert!(
            body.contains("ifletSome(vk)=requested{crate::components::app::mark_needs_sync(vk);}"),
            "mark_room_subscribed must request the pass it was told to: {body}"
        );
    }

    /// No pass is requested when nothing was held back, so a reconnect that
    /// re-subscribes every room does not queue a pass per room.
    #[test]
    fn subscribing_with_nothing_held_back_requests_no_pass() {
        let mut si = SyncInfo::new();
        let room = test_owner(41);
        si.register_new_room(room);
        si.update_sync_status(&room, RoomSyncStatus::Subscribing);
        let state = ChatRoomStateV1::default();
        si.update_last_synced_state(&room, &state);
        // A pass that finds nothing to send holds nothing back.
        assert!(!si.room_needs_update(&room, &state));
        assert!(!si.mark_subscribed(&room));
        // Unknown room: nothing to mark.
        assert!(!si.mark_subscribed(&test_owner(42)));
    }

    /// Every response handler that marks a room `Subscribed` goes through
    /// `mark_room_subscribed`, the one place that requests the held pass. A
    /// handler writing the status directly would lose the first message of a
    /// fresh join again.
    #[test]
    fn response_handlers_mark_subscribed_through_the_helper() {
        for (file, src) in [
            (
                "put_response.rs",
                include_str!("freenet_api/response_handler/put_response.rs"),
            ),
            (
                "subscribe_response.rs",
                include_str!("freenet_api/response_handler/subscribe_response.rs"),
            ),
            (
                "get_response.rs",
                include_str!("freenet_api/response_handler/get_response.rs"),
            ),
        ] {
            let prod: String = src
                .split("#[cfg(test)]")
                .next()
                .unwrap()
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !prod.contains("RoomSyncStatus::Subscribed"),
                "{file} writes Subscribed directly; use mark_room_subscribed"
            );
            assert!(
                prod.lines().any(|l| l.trim_start().starts_with(
                    "crate::components::app::sync_info::mark_room_subscribed(owner_vk);"
                )),
                "{file} must call mark_room_subscribed"
            );
        }
    }

    fn test_owner(seed: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    /// freenet/river#556: a `Subscribed` room must be re-armed for
    /// re-subscription after a reconnect, because the node dropped that
    /// subscription with the old socket. Without this the tab reconnects,
    /// reports "Connected", and silently receives nothing thereafter.
    #[test]
    fn reconnect_restales_subscribed_rooms_so_they_are_resubscribed() {
        let mut si = SyncInfo::new();
        let subscribed = test_owner(1);
        si.register_new_room(subscribed);
        si.update_sync_status(&subscribed, RoomSyncStatus::Subscribed);

        // Before: a Subscribed room is invisible to the re-subscribe sweep.
        assert_eq!(
            si.get_sync_status(&subscribed),
            Some(&RoomSyncStatus::Subscribed)
        );

        assert_eq!(si.mark_subscribed_rooms_disconnected(), 1);
        assert_eq!(
            si.get_sync_status(&subscribed),
            Some(&RoomSyncStatus::Disconnected),
            "a Subscribed room must be re-armed after a reconnect, or nothing \
             ever re-subscribes it"
        );
    }

    /// Only `Subscribed` is flipped. `Subscribing` keeps its timeout bookkeeping,
    /// and terminal `Error` (a room that exhausted its #290 retry budget) must not
    /// be silently revived by an unrelated reconnect.
    #[test]
    fn reconnect_restale_leaves_other_statuses_alone() {
        let mut si = SyncInfo::new();
        let (subscribing, errored, disconnected) = (test_owner(2), test_owner(3), test_owner(4));
        for k in [&subscribing, &errored, &disconnected] {
            si.register_new_room(*k);
        }
        si.update_sync_status(&subscribing, RoomSyncStatus::Subscribing);
        let gave_up = ErrorReport::new(LABEL_ROOM_NOT_FOUND, "gave up", "gave up");
        si.update_sync_status(&errored, RoomSyncStatus::Error(gave_up.clone()));
        si.update_sync_status(&disconnected, RoomSyncStatus::Disconnected);

        assert_eq!(
            si.mark_subscribed_rooms_disconnected(),
            0,
            "nothing was Subscribed, so nothing should have been flipped"
        );
        assert_eq!(
            si.get_sync_status(&subscribing),
            Some(&RoomSyncStatus::Subscribing)
        );
        assert!(
            si.map
                .get(&subscribing)
                .unwrap()
                .subscribing_since
                .is_some(),
            "flipping must not clear a Subscribing room's timeout stamp"
        );
        assert_eq!(
            si.get_sync_status(&errored),
            Some(&RoomSyncStatus::Error(gave_up)),
            "a terminal Error room must not be revived by a reconnect"
        );
    }

    /// Re-arming must not consume the #290 retry budget: `update_sync_status`
    /// clears `failed_sync_attempts` only on `Subscribed`, so a room that has
    /// been flipped keeps whatever budget it had.
    #[test]
    fn reconnect_restale_does_not_spend_retry_budget() {
        let mut si = SyncInfo::new();
        let owner = test_owner(5);
        si.register_new_room(owner);
        si.update_sync_status(&owner, RoomSyncStatus::Subscribed);
        // A successful subscribe zeroes the counter; set a streak afterwards to
        // prove the flip preserves it rather than incrementing it.
        si.map.get_mut(&owner).unwrap().failed_sync_attempts = 2;

        si.mark_subscribed_rooms_disconnected();

        assert_eq!(
            si.map.get(&owner).unwrap().failed_sync_attempts,
            2,
            "re-arming after a reconnect is not a failed attempt"
        );
    }

    /// `touch_subscribing_since` refreshes the timestamp for a `Subscribing`
    /// room — this is what keeps a long backward-probe recovery out of the
    /// `rooms_awaiting_subscription` timeout sweep (freenet/river#292) — but
    /// must NOT touch a room in any other status (no spurious status flap,
    /// and a `Subscribed` room is not swept anyway).
    #[test]
    fn touch_subscribing_since_only_restamps_while_subscribing() {
        let mut si = SyncInfo::new();
        let owner = test_owner(1);
        si.register_new_room(owner);

        // Subscribing room with a cleared timestamp → touch re-stamps it.
        si.update_sync_status(&owner, RoomSyncStatus::Subscribing);
        si.map.get_mut(&owner).unwrap().subscribing_since = None;
        si.touch_subscribing_since(&owner);
        assert!(
            si.map.get(&owner).unwrap().subscribing_since.is_some(),
            "touch must re-stamp a Subscribing room"
        );

        // Subscribed room → touch must NOT re-stamp and must NOT flip status.
        si.update_sync_status(&owner, RoomSyncStatus::Subscribed);
        assert!(si.map.get(&owner).unwrap().subscribing_since.is_none());
        si.touch_subscribing_since(&owner);
        assert!(
            si.map.get(&owner).unwrap().subscribing_since.is_none(),
            "touch must not act on a non-Subscribing room"
        );
        assert_eq!(
            si.map.get(&owner).unwrap().sync_status,
            RoomSyncStatus::Subscribed,
            "touch must never change the status"
        );
    }

    /// freenet/river#290: a room STILL awaiting its initial sync (placeholder
    /// state, contract possibly absent from the network) must NOT retry forever.
    /// After `MAX_SYNC_ATTEMPTS_BEFORE_ERROR` failed attempts it is promoted to a
    /// terminal `Error` and `record_failed_sync_attempt` returns `false` (stop
    /// retrying). Up to that point it stays `Disconnected` (retry) so the
    /// generous bound preserves the freenet-core#1470 race handling.
    #[test]
    fn record_failed_sync_attempt_bounds_initial_sync_then_errors() {
        let mut si = SyncInfo::new();
        let owner = test_owner(7);
        si.register_new_room(owner);

        // All but the last attempt: keep retrying, status stays Disconnected.
        for attempt in 1..MAX_SYNC_ATTEMPTS_BEFORE_ERROR {
            let should_retry = si.record_failed_sync_attempt(&owner, true);
            assert!(should_retry, "attempt {attempt} (< bound) must still retry");
            assert_eq!(
                si.map.get(&owner).unwrap().sync_status,
                RoomSyncStatus::Disconnected,
                "attempt {attempt} must reset to Disconnected for retry"
            );
            assert_eq!(si.map.get(&owner).unwrap().failed_sync_attempts, attempt);
        }

        // The bound-th attempt gives up: terminal Error, stop retrying.
        let should_retry = si.record_failed_sync_attempt(&owner, true);
        assert!(!should_retry, "reaching the bound must stop retrying");
        assert!(
            matches!(si.get_sync_status(&owner), Some(RoomSyncStatus::Error(_))),
            "reaching the bound must promote the room to terminal Error"
        );
        assert!(
            si.map.get(&owner).unwrap().subscribing_since.is_none(),
            "giving up must clear the subscribing timestamp"
        );

        // Further failures keep the room terminal and never re-arm a retry.
        assert!(!si.record_failed_sync_attempt(&owner, true));
        assert!(matches!(
            si.get_sync_status(&owner),
            Some(RoomSyncStatus::Error(_))
        ));
    }

    /// A room that already holds valid synced state (`awaiting_initial_sync ==
    /// false`) must NEVER be given up on — a transient outage must not strand a
    /// working room (codex review of PR #360). It keeps resetting to
    /// `Disconnected` so the existing self-healing re-PUT continues, no matter
    /// how many times it fails.
    #[test]
    fn record_failed_sync_attempt_never_errors_a_synced_room() {
        let mut si = SyncInfo::new();
        let owner = test_owner(13);
        si.register_new_room(owner);

        for _ in 0..(MAX_SYNC_ATTEMPTS_BEFORE_ERROR * 3) {
            let should_retry = si.record_failed_sync_attempt(&owner, false);
            assert!(
                should_retry,
                "a synced room must always keep retrying, never give up"
            );
            assert_eq!(
                si.map.get(&owner).unwrap().sync_status,
                RoomSyncStatus::Disconnected,
                "a synced room must stay Disconnected, never reach Error"
            );
        }
        assert!(
            !matches!(si.get_sync_status(&owner), Some(RoomSyncStatus::Error(_))),
            "a synced room must never be promoted to terminal Error"
        );
    }

    /// A successful subscription clears the failed-attempt budget so a later
    /// transient failure starts counting from zero again rather than tipping a
    /// long-lived room straight into the terminal Error state.
    #[test]
    fn successful_subscription_resets_failure_budget() {
        let mut si = SyncInfo::new();
        let owner = test_owner(9);
        si.register_new_room(owner);

        // Burn most of the budget without reaching the bound.
        for _ in 1..MAX_SYNC_ATTEMPTS_BEFORE_ERROR {
            assert!(si.record_failed_sync_attempt(&owner, true));
        }
        assert_eq!(
            si.map.get(&owner).unwrap().failed_sync_attempts,
            MAX_SYNC_ATTEMPTS_BEFORE_ERROR - 1
        );

        // A successful sync resets the counter.
        si.update_sync_status(&owner, RoomSyncStatus::Subscribed);
        assert_eq!(si.map.get(&owner).unwrap().failed_sync_attempts, 0);

        // So the next failure starts over and does NOT immediately error.
        assert!(si.record_failed_sync_attempt(&owner, true));
        assert_eq!(
            si.map.get(&owner).unwrap().sync_status,
            RoomSyncStatus::Disconnected
        );
    }

    /// freenet/river#757: a room loaded from delegate storage subscribes with a
    /// code-free GET; a room this session created (never marked known) still
    /// seeds with the PUT, because the network cannot have it yet.
    #[test]
    fn only_rooms_known_on_the_network_subscribe_by_get() {
        let mut si = SyncInfo::new();
        let (loaded, created) = (test_owner(20), test_owner(21));
        si.mark_known_on_network(loaded);
        si.register_new_room(loaded);
        si.register_new_room(created);
        assert_eq!(si.subscribe_route(&loaded), SubscribeRoute::Get);
        assert_eq!(si.subscribe_route(&created), SubscribeRoute::Put);
    }

    /// freenet/river#757, the safety net: when the subscribe GET finds the
    /// contract missing, the room must be re-seeded with the full PUT, and keep
    /// PUTting until a subscription succeeds. Then a later re-subscribe (a
    /// reconnect) goes back to the cheap GET.
    #[test]
    fn not_found_on_a_subscribe_get_falls_back_to_the_seeding_put() {
        let mut si = SyncInfo::new();
        let owner = test_owner(22);
        si.mark_known_on_network(owner);
        si.register_new_room(owner);

        si.note_subscribe_get_sent(&owner);
        si.update_sync_status(&owner, RoomSyncStatus::Subscribing);
        assert!(
            si.subscribe_get_found_nothing(&owner),
            "a NotFound answering our subscribe GET must ask for a re-seed"
        );
        assert_eq!(
            si.subscribe_route(&owner),
            SubscribeRoute::Put,
            "a room the network lost must be re-seeded with the PUT"
        );

        // The PUT itself can fail and be retried: still a PUT, never the GET.
        assert!(si.record_failed_sync_attempt(&owner, false));
        assert_eq!(si.subscribe_route(&owner), SubscribeRoute::Put);

        // The PUT lands and the room subscribes: the network has it again.
        si.update_sync_status(&owner, RoomSyncStatus::Subscribed);
        assert_eq!(si.subscribe_route(&owner), SubscribeRoute::Get);
    }

    /// A subscribe GET that times out (or errors with no request id) says
    /// nothing about whether the network has the room, so the retry must be the
    /// PUT that works either way — exactly the pre-#757 behaviour.
    #[test]
    fn a_timed_out_subscribe_get_retries_with_the_seeding_put() {
        let mut si = SyncInfo::new();
        let owner = test_owner(23);
        si.mark_known_on_network(owner);
        si.register_new_room(owner);
        si.note_subscribe_get_sent(&owner);
        si.update_sync_status(&owner, RoomSyncStatus::Subscribing);

        assert!(si.record_failed_sync_attempt(&owner, false));
        assert_eq!(si.subscribe_route(&owner), SubscribeRoute::Put);
        assert!(
            !si.take_subscribe_get(&owner),
            "the timed-out GET is no longer outstanding"
        );
    }

    /// Only an OUTSTANDING subscribe GET reacts. A `NotFound` or `GetResponse`
    /// for any other GET (an invitation, an imported room, a refresh) must not
    /// flip a room to the PUT route or be mistaken for a subscription, and an
    /// answer is consumed once.
    #[test]
    fn subscribe_get_answers_are_matched_once_and_only_when_outstanding() {
        let mut si = SyncInfo::new();
        let owner = test_owner(24);
        si.mark_known_on_network(owner);
        si.register_new_room(owner);

        assert!(!si.subscribe_get_found_nothing(&owner));
        assert!(!si.take_subscribe_get(&owner));
        assert_eq!(si.subscribe_route(&owner), SubscribeRoute::Get);

        si.note_subscribe_get_sent(&owner);
        assert!(si.take_subscribe_get(&owner));
        assert!(
            !si.take_subscribe_get(&owner),
            "consumed by the first answer"
        );
        assert!(
            !si.subscribe_get_found_nothing(&owner),
            "a late NotFound after the GET already answered must not force a PUT"
        );
        assert_eq!(si.subscribe_route(&owner), SubscribeRoute::Get);
    }

    /// A refused subscription (`SubscribeResponse { subscribed: false }`) puts
    /// a known room back on the seeding PUT, as before the GET route existed.
    #[test]
    fn a_refused_subscription_requires_the_seeding_put() {
        let mut si = SyncInfo::new();
        let owner = test_owner(26);
        si.mark_known_on_network(owner);
        si.register_new_room(owner);
        assert_eq!(si.subscribe_route(&owner), SubscribeRoute::Get);
        si.require_seed_put(&owner);
        assert_eq!(si.subscribe_route(&owner), SubscribeRoute::Put);
    }

    /// A room created this session joins the GET route once it has subscribed,
    /// so a reconnect does not re-upload the contract for it either.
    #[test]
    fn a_subscribed_room_is_known_on_the_network() {
        let mut si = SyncInfo::new();
        let owner = test_owner(25);
        si.register_new_room(owner);
        assert_eq!(si.subscribe_route(&owner), SubscribeRoute::Put);
        si.update_sync_status(&owner, RoomSyncStatus::Subscribed);
        assert_eq!(si.subscribe_route(&owner), SubscribeRoute::Get);
    }

    /// `record_failed_sync_attempt` on an unknown room is a no-op that reports
    /// "do not retry" rather than panicking.
    #[test]
    fn record_failed_sync_attempt_on_unknown_room_is_noop() {
        let mut si = SyncInfo::new();
        let owner = test_owner(11);
        assert!(!si.record_failed_sync_attempt(&owner, true));
        assert!(si.get_sync_status(&owner).is_none());
    }
}
