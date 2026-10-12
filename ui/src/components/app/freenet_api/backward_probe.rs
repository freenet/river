//! Backward-probe recovery for rooms stranded under an older room-contract
//! generation (freenet/river#292).
//!
//! The room contract key is `BLAKE3(room_contract.wasm, params)`. Every
//! room-contract WASM upgrade moves the key for every owner, so a room that
//! was dormant across one or more upgrades has its live state stranded under
//! an older-generation contract key.
//!
//! ## Design principle (the same gating the chat delegate uses — #253)
//!
//! **The current contract key is always authoritative.** The backward probe
//! fires ONLY when the current key has no usable state:
//!
//! * Current key has state → adopt it. Never probe backward.
//! * Current key empty/absent → probe legacy keys newest-to-oldest. The first
//!   that returns real state is the room's last-active state; adopt it AND PUT
//!   it forward onto the current key so the room is no longer stranded.
//! * Neither current nor any legacy key has state → only then is the device's
//!   local snapshot PUT to seed the current key.
//!
//! ## The decision driver (freenet/river#398 phase 2)
//!
//! The probe *decisions* — candidate order (newest-first), what counts as a
//! hit, when to advance, when to stop, what to adopt — live in
//! `freenet_migrate`'s sans-IO [`ProbeDriver`]. This module is the thin I/O
//! adapter: it pumps GETs and timeouts through the driver and runs the app's
//! recover-or-seed side effects on the terminal [`Outcome`]. River's
//! app-specific semantics (CBOR decode, "is this real state", CRDT-merge with
//! the local snapshot) are supplied by [`RiverProbeOps`].
//!
//! The driver's per-candidate correlation is single-shot WITHIN a single
//! probe. It does NOT cover cross-probe reuse: sequential probes for the SAME
//! owner walk the SAME deterministic legacy ids, so a later probe re-registers
//! an id an earlier probe's watchdog is still armed against. Each probe GET
//! therefore carries a monotonic **per-fire token** ([`PROBE_ROUTES`] value):
//! a watchdog advances its probe only if the route it armed against still
//! carries its own token, so a stale watchdog from a completed probe can never
//! advance a LATER probe that reused the same id. The token replaces the old
//! hand-rolled per-fire epoch counter; the driver's single-shot correlation
//! covers everything else.
//!
//! Two paths can start a second same-owner probe, with different timing:
//!
//! * The **subscription-timeout sweep** (`rooms_awaiting_subscription`) is
//!   serialized out of the race by construction: a probe re-stamps
//!   `subscribing_since` every hop and `PROBE_GET_TIMEOUT` (12s) is below
//!   `REPUT_DELAY_MS` (20s, the sweep threshold), so the sweep never reclaims a
//!   room and re-GETs it mid-probe. That ordering is what the compile-time
//!   assert below pins.
//! * The **reconnect path** is NOT serialized: on a WebSocket reconnect
//!   `room_synchronizer` sets the room `Disconnected` and clears
//!   `subscribing_since`, and `rooms_awaiting_subscription` then re-GETs with no
//!   throttle — so a fresh current-key GET (and a second probe) can start well
//!   inside the first probe's 12s window. This is the path the per-fire token
//!   exists to cover; without it the first probe's late watchdog would advance
//!   the second probe past its newest generation.
//!
//! ## Routing & bookkeeping
//!
//! A GET response for a *legacy* contract key cannot be resolved back to an
//! `owner_vk` via `SYNC_INFO` (keyed by the current contract id) or via
//! `RoomData::contract_key` (also the current id). [`PROBE_ROUTES`] is the
//! side-table that maps the outstanding legacy `ContractInstanceId` back to the
//! `(owner_vk, per-fire token)` of the probe GET it belongs to; [`PROBE_DRIVERS`]
//! holds one in-flight [`ProbeDriver`] per owner.
//!
//! Both are plain `Mutex`-guarded maps, NOT Dioxus signals: internal
//! bookkeeping with zero UI reactivity (no component or memo ever reads them),
//! so they must not carry signal semantics. This is the same shape as
//! `chat_delegate::ENSURE_SUBSCRIPTION_SENT`. Using plain `Mutex`es means
//! mutations need no `defer()` and cannot cause a re-entrant signal borrow.
//!
//! ## Liveness — every probe GET has a watchdog
//!
//! A legacy generation whose contract has been garbage-collected from the
//! network may never produce a `GetResponse`. Every probe GET is therefore
//! paired with a [`PROBE_GET_TIMEOUT`] watchdog: if the response has not
//! arrived by then, the watchdog counts the generation as a miss so the driver
//! advances to the next generation (and ultimately seeds the current key)
//! rather than stalling forever. Without this a dormant room — exactly what
//! this feature targets — could be left permanently stuck. Within one probe,
//! whichever of {response, watchdog} removes the route first owns the hop; the
//! loser finds the route gone and no-ops. Across probes, the per-fire token
//! (above) stops a stale watchdog from claiming a later probe's re-registered
//! route.
//!
//! A typed `NotFound` for a probe GET is an ANSWER, not silence: it advances
//! the walk at once instead of waiting out the watchdog. Only the watchdog (and
//! a failed send, which it covers) leaves a generation **unresolved**. The 0.5
//! driver counts both as a miss, so the adapter keeps that distinction itself
//! ([`ProbeEntry::unresolved`]); it decides whether a re-key merge may be
//! recorded as finished.
//!
//! ## Re-key merge (freenet/river#769)
//!
//! When the bundled room-contract WASM changes, every room's key moves and the
//! new key starts empty. The device's local snapshot is not the whole room:
//! messages that reached the OLD key after this device last saved exist only
//! there. So for a room whose stored contract key is from an older generation,
//! [`start_rekey_merge`] walks the same driver over the generations from the
//! newest legacy one down to the one this device last synced under, merges what
//! it finds into `ROOMS` (CRDT merge, never a replace), and only then lets the
//! room's seed PUT go out, carrying the merged state. The room's
//! `previous_contract_key` is the durable "merge still owed" marker: it is
//! cleared only once every candidate answered, so a timed-out old key is
//! retried on a later load.

use crate::components::app::freenet_api::constants::REPUT_DELAY_MS;
use crate::components::app::freenet_api::response_handler::get_response::{
    adopt_recovered_probe_state, merge_recovered_into_room, merge_room_states, request_seed_put,
    seed_current_key_with_local,
};
use crate::components::app::sync_info::SYNC_INFO;
use crate::components::app::ROOMS;
use crate::util::{owner_vk_to_legacy_contract_keys, safe_spawn_local, sleep, try_from_cbor_slice};
use dioxus::logger::tracing::{info, warn};
use ed25519_dalek::VerifyingKey;
use freenet_migrate::{
    NewestFirst, Outcome, ProbeDriver, ProbeStateOps, SelectionPolicy, Step, DEFAULT_MAX_PROBE_HOPS,
};
use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
use freenet_stdlib::prelude::ContractInstanceId;
use river_core::room_state::member::MemberId;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

/// River caps a probe walk at 64 legacy generations (defence-in-depth against a
/// runaway chain if the legacy registry ever grows unexpectedly large). That
/// cap is now the driver's default hop count; pin the two together so River's
/// long-standing bound cannot silently drift if the crate default changes.
const _: () = assert!(
    DEFAULT_MAX_PROBE_HOPS == 64,
    "River relies on the 64-hop backward-probe cap; if freenet_migrate's default \
     changed, set it explicitly via ProbeDriver::with_max_hops"
);

/// How long to wait for a probe GET response before treating the legacy
/// generation as absent and advancing. An existing contract responds well
/// inside this; only a garbage-collected one runs the timeout down.
const PROBE_GET_TIMEOUT: Duration = Duration::from_secs(12);

// Load-bearing invariant: a probe re-stamps `subscribing_since` every hop, and
// consecutive hops are at most `PROBE_GET_TIMEOUT` apart (the watchdog). That
// only keeps the room out of `rooms_awaiting_subscription`'s sweep if a hop is
// shorter than the sweep's `REPUT_DELAY_MS` threshold. Pinned at compile time.
const _: () = assert!(
    (PROBE_GET_TIMEOUT.as_millis() as u64) < REPUT_DELAY_MS,
    "PROBE_GET_TIMEOUT must stay below REPUT_DELAY_MS or a probe is reclaimed mid-walk"
);

/// River's app-supplied state semantics for the sans-IO decision driver: how to
/// decode a legacy GET response, whether it is *real* state, and how to fold it
/// with the device's local snapshot. Everything else (candidate order,
/// single-shot correlation, exhaustion) is owned by the driver.
struct RiverProbeOps {
    /// The owner whose stranded room this probe is recovering.
    owner_vk: VerifyingKey,
}

impl ProbeStateOps for RiverProbeOps {
    type State = ChatRoomStateV1;

    fn decode(&self, bytes: &[u8]) -> Option<ChatRoomStateV1> {
        // Defensive decode: the probe walks many historical generations, and a
        // very old one may carry a `ChatRoomStateV1` layout the current type
        // cannot decode. `None` marks the candidate a miss so the driver
        // advances rather than panicking — matching the CLI's `try_get_state`.
        try_from_cbor_slice::<ChatRoomStateV1>(bytes)
    }

    fn is_real(&self, state: &ChatRoomStateV1) -> bool {
        // "Real" == the configuration signature verifies against the owner (a
        // default/placeholder state is signed by the all-zero key and fails).
        // This is the same predicate as `RoomData::is_awaiting_initial_sync`
        // and the current-key probe-start gate.
        state.configuration.verify_signature(&self.owner_vk).is_ok()
    }

    fn merge_with_local(
        &self,
        recovered: ChatRoomStateV1,
        local: &ChatRoomStateV1,
    ) -> ChatRoomStateV1 {
        // CRDT-merge the recovered legacy state (primary) with the device's
        // local snapshot BEFORE it is PUT forward, so unsynced local edits the
        // device made offline are not dropped. On a genuine merge failure this
        // keeps the recovered state alone (the shipped keep-primary behavior).
        let params = ChatRoomParametersV1 {
            owner: self.owner_vk,
        };
        merge_room_states(recovered, local, &params)
    }

    // `prepare_forward` stays the driver default (identity) on purpose: River
    // adopts the UNSTRIPPED merged state locally and strips the upgrade pointer
    // ONLY inside `put_state_to_current_key` (freenet/river#427). Overriding it
    // here would strip too early and change what is adopted locally — do NOT
    // add a prepare_forward override; the strip belongs on the forward PUT.
}

/// Maps the `ContractInstanceId` of the currently-outstanding legacy probe GET
/// back to the `(owner_vk, per-fire token)` of the probe GET it belongs to.
/// Plain `Mutex` map — see the module docs. The presence of an entry is what
/// makes a legacy-key GET response route into the probe handler
/// ([`is_probe_instance`]); the token lets a watchdog tell its own outstanding
/// fire apart from a later probe that reused the same deterministic id.
static PROBE_ROUTES: LazyLock<Mutex<HashMap<ContractInstanceId, (VerifyingKey, u64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Monotonic source of the per-fire route token. Bumped once per
/// [`fire_probe_get`]; the value stored in [`PROBE_ROUTES`] and captured by that
/// fire's watchdog. Globally unique, so a token match implies the SAME fire.
static PROBE_FIRE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh, globally-unique per-fire token for one probe GET.
fn next_fire_token() -> u64 {
    PROBE_FIRE_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// What a probe is for, which decides what its terminal [`Outcome`] does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbePurpose {
    /// freenet/river#292: the current key came back without real state.
    /// Recover-or-seed by PUTting the outcome straight onto the current key.
    Recover,
    /// freenet/river#769: the room was last synced under the older
    /// generation `previous`. Merge what the older keys hold into `ROOMS`;
    /// then, if `seed`, re-seed the current key through the ordinary seed-PUT
    /// path, so the PUT carries the merged state.
    RekeyMerge {
        previous: ContractInstanceId,
        seed: bool,
    },
}

/// One owner's in-flight probe.
struct ProbeEntry {
    driver: ProbeDriver<RiverProbeOps>,
    /// Candidates whose GET was never answered (watchdog fired; a failed send
    /// lands here too). Their state is unknown, which the 0.5 driver cannot
    /// express: it treats a timeout as a miss. Kept here so a re-key merge
    /// that heard nothing from a generation is never recorded as finished.
    unresolved: Vec<ContractInstanceId>,
    purpose: ProbePurpose,
}

/// One in-flight probe per owner. The driver owns the sans-IO probe decision
/// state; this module only pumps I/O through it. Deduplicating concurrent
/// probes for one owner is our job (see [`start_backward_probe`]).
static PROBE_DRIVERS: LazyLock<Mutex<HashMap<VerifyingKey, ProbeEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `(owner, previous generation)` pairs a re-key merge has been started for
/// on this page load. A merge runs at most once per load; one that could not
/// finish is retried on the NEXT load, from the persisted marker.
static REKEY_ATTEMPTED: LazyLock<Mutex<HashSet<(VerifyingKey, ContractInstanceId)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Lock the route table, recovering from a poisoned mutex (a panic while the
/// lock was held must not wedge all future recovery).
fn routes() -> MutexGuard<'static, HashMap<ContractInstanceId, (VerifyingKey, u64)>> {
    PROBE_ROUTES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Remove `id`'s route ONLY if it still carries `(owner_vk, token)` — the
/// per-fire guard a watchdog uses to advance its probe. Returns `true` (route
/// removed) only when this watchdog owns the CURRENT outstanding fire for `id`.
///
/// The check-and-remove is one locked step: a stale watchdog whose token no
/// longer matches (a later same-owner probe re-fired the same deterministic id
/// with a fresh token) leaves the route untouched and does not advance the
/// later probe. This is what the deleted per-fire epoch counter guarded.
fn claim_route_if_current(id: ContractInstanceId, owner_vk: VerifyingKey, token: u64) -> bool {
    let mut routes = routes();
    match routes.get(&id) {
        Some(&(o, t)) if o == owner_vk && t == token => {
            routes.remove(&id);
            true
        }
        _ => false,
    }
}

/// Lock the driver table, recovering from a poisoned mutex.
fn drivers() -> MutexGuard<'static, HashMap<VerifyingKey, ProbeEntry>> {
    PROBE_DRIVERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Lock the re-key attempt latch, recovering from a poisoned mutex.
fn rekey_attempted() -> MutexGuard<'static, HashSet<(VerifyingKey, ContractInstanceId)>> {
    REKEY_ATTEMPTED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The generations a re-key merge asks, newest first: every legacy generation
/// from the newest down to and including `previous`, the one this device last
/// synced the room under (freenet/river#769).
///
/// Nothing older is asked. This device's snapshot already holds what it saw
/// under `previous`, so an older key can only add what was written there
/// before this device moved on, which is the #292 dormant-room recovery's job,
/// not a re-key's. Bounding the list is what keeps the seed PUT prompt: one
/// GET in the ordinary case, one per skipped release otherwise.
///
/// If `previous` is not in the registry at all (a development build, say),
/// only `previous` itself is asked: it is the one key known to hold this
/// device's room.
pub(crate) fn rekey_candidates(
    legacy_newest_first: &[ContractInstanceId],
    previous: ContractInstanceId,
) -> Vec<ContractInstanceId> {
    match legacy_newest_first.iter().position(|id| *id == previous) {
        Some(pos) => legacy_newest_first[..=pos].to_vec(),
        None => vec![previous],
    }
}

/// What a finished re-key merge does (freenet/river#769). Pure, so each arm is
/// testable without a runtime.
#[derive(Debug)]
pub(crate) struct RekeyFinish {
    /// Older-generation state to CRDT-merge into the room, if any was found.
    pub recovered: Option<ChatRoomStateV1>,
    /// Every candidate answered, so the marker may be cleared (once any
    /// recovered state has actually merged).
    pub complete: bool,
}

pub(crate) fn plan_rekey_finish(
    outcome: Outcome<ChatRoomStateV1>,
    unresolved: &[ContractInstanceId],
) -> RekeyFinish {
    let complete = unresolved.is_empty();
    match outcome {
        Outcome::Recovered { merged, .. } => RekeyFinish {
            recovered: Some(merged),
            complete,
        },
        // Every generation answered without real state. The device's own
        // snapshot is already in ROOMS, so there is nothing to merge.
        Outcome::SeedLocal { .. } | Outcome::NoLegacy { .. } => RekeyFinish {
            recovered: None,
            complete,
        },
    }
}

/// Outcome of asking for a re-key merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RekeyStart {
    /// A merge walk is now running.
    Started,
    /// One was already running for this owner (asked to seed if requested).
    AlreadyRunning,
    /// One already ran for this generation on this page load. The caller
    /// proceeds without it; the next load retries if it did not finish.
    AlreadyAttempted,
}

/// Merge what the older room-contract generations hold for `owner_vk`'s room
/// into `ROOMS` (freenet/river#769), then, if `seed`, re-seed the current key.
///
/// `previous` is the contract id the room was last synced under (its persisted
/// `previous_contract_key`). With `seed`, the caller must hold the room out of
/// the subscribe sweep (status `Subscribing`) until this finishes: the seed PUT
/// is the merge's last step, so it carries the old key's messages rather than
/// only this device's snapshot.
pub async fn start_rekey_merge(
    owner_vk: VerifyingKey,
    previous: ContractInstanceId,
    local_snapshot: ChatRoomStateV1,
    seed: bool,
) -> RekeyStart {
    {
        let mut drivers = drivers();
        if let Some(entry) = drivers.get_mut(&owner_vk) {
            // A `Recover` probe seeds on its own; a background merge is told to.
            if seed {
                if let ProbePurpose::RekeyMerge { seed: s, .. } = &mut entry.purpose {
                    *s = true;
                }
            }
            return RekeyStart::AlreadyRunning;
        }
    }
    if !rekey_attempted().insert((owner_vk, previous)) {
        return RekeyStart::AlreadyAttempted;
    }

    let legacy: Vec<ContractInstanceId> = owner_vk_to_legacy_contract_keys(&owner_vk)
        .iter()
        .map(|k| *k.id())
        .collect();
    let candidates = rekey_candidates(&legacy, previous);
    info!(
        "Re-key merge for room {:?}: asking {} older generation(s) before {} (freenet/river#769)",
        MemberId::from(owner_vk),
        candidates.len(),
        if seed {
            "seeding the new key"
        } else {
            "merging in the background"
        }
    );
    // `rekey_candidates` keeps the registry's newest-first order (see
    // `start_backward_probe` for why that order is the anti-rollback guarantee).
    let driver = ProbeDriver::new(
        RiverProbeOps { owner_vk },
        local_snapshot,
        NewestFirst::assume_ordered(candidates),
        SelectionPolicy::NewestFirstWins,
    );
    drivers().insert(
        owner_vk,
        ProbeEntry {
            driver,
            unresolved: Vec::new(),
            purpose: ProbePurpose::RekeyMerge { previous, seed },
        },
    );
    pump_probe(owner_vk).await;
    RekeyStart::Started
}

/// Whether `instance_id` is the contract id of an outstanding backward-probe
/// GET. Used by `handle_get_response` to route legacy-key responses.
pub fn is_probe_instance(instance_id: &ContractInstanceId) -> bool {
    routes().contains_key(instance_id)
}

/// Start a backward probe for `owner_vk`'s room: build the driver over the
/// owner's legacy generations (newest-first) and pump the first GET. Called
/// when the current-key GET came back empty.
///
/// `local_snapshot` is the device's current room state, captured by the caller;
/// it is moved into the driver for the merge / last-resort seed.
///
/// Returns `true` if a probe is in flight (one was started, or one was already
/// running for this owner) — the caller must then NOT seed the current key; the
/// probe owns recovery-or-seed. Returns `false` only when there are no legacy
/// generations at all, in which case the caller falls back to seeding the
/// current key itself.
pub async fn start_backward_probe(owner_vk: VerifyingKey, local_snapshot: ChatRoomStateV1) -> bool {
    let legacy_keys = owner_vk_to_legacy_contract_keys(&owner_vk);
    if legacy_keys.is_empty() {
        info!(
            "No legacy room-contract generations — skipping backward probe for {:?}",
            MemberId::from(owner_vk)
        );
        return false;
    }

    // Don't start a second probe for an owner that already has one running.
    // Return `true` so the caller still treats recovery as in-flight. A probe's
    // driver entry lives from here until its terminal Outcome (removed in
    // `pump_probe`'s Done branch), so a completed probe leaves no entry and a
    // later probe for the same owner can start fresh.
    //
    // A running re-key merge (freenet/river#769) covers this room's newer
    // generations; the current key is empty, so it must now end in a seed.
    if let Some(entry) = drivers().get_mut(&owner_vk) {
        if let ProbePurpose::RekeyMerge { seed, .. } = &mut entry.purpose {
            *seed = true;
        }
        info!(
            "Backward probe already in progress for {:?}, not restarting",
            MemberId::from(owner_vk)
        );
        return true;
    }

    info!(
        "Starting backward probe for {:?}: {} legacy generation(s) to try",
        MemberId::from(owner_vk),
        legacy_keys.len()
    );

    // `owner_vk_to_legacy_contract_keys` is newest-first BY CONSTRUCTION — it
    // reverses the oldest-first registry (see `legacy_contract_keys_for_owner`)
    // — so `assume_ordered` is sound. The whole anti-rollback guarantee rests
    // on this order: probing newest-first and stopping at the first real state
    // means an older generation can never shadow a newer one.
    let candidates = NewestFirst::assume_ordered(legacy_keys.iter().map(|k| *k.id()).collect());
    let driver = ProbeDriver::new(
        RiverProbeOps { owner_vk },
        local_snapshot,
        candidates,
        // River adopts exactly one generation (the newest real one) and never
        // reads older ones — safe for delete-by-absence state (pruned messages
        // stay pruned). The 64-hop cap is the driver default (asserted above).
        SelectionPolicy::NewestFirstWins,
    );
    drivers().insert(
        owner_vk,
        ProbeEntry {
            driver,
            unresolved: Vec::new(),
            purpose: ProbePurpose::Recover,
        },
    );
    pump_probe(owner_vk).await;
    true
}

/// Deliver a typed `NotFound` for a legacy-key probe GET: the node answered
/// that it holds nothing under that key. That is an answer, so the walk
/// advances now rather than after the watchdog, and the generation is NOT
/// recorded as unresolved. (The 0.5 driver's `on_timeout` is its only "miss"
/// event; the unknown-vs-absent distinction lives in `ProbeEntry::unresolved`.)
pub(crate) async fn deliver_probe_absent(id: ContractInstanceId) {
    let Some((owner_vk, _token)) = routes().remove(&id) else {
        return;
    };
    info!("Legacy contract {id} answered NotFound — advancing to older generation");
    record_probe_answer(owner_vk, id, ProbeAnswer::Absent);
    pump_probe(owner_vk).await;
}

/// How a probe GET ended, when it ended without state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeAnswer {
    /// The node answered `NotFound`.
    Absent,
    /// No answer before the watchdog fired.
    Silent,
}

/// Feed a stateless end of `owner_vk`'s probe GET for `id` into its entry. Both
/// advance the walk (the 0.5 driver's `on_timeout` is its only miss event);
/// only silence marks the generation unresolved.
fn record_probe_answer(owner_vk: VerifyingKey, id: ContractInstanceId, answer: ProbeAnswer) {
    if let Some(entry) = drivers().get_mut(&owner_vk) {
        if answer == ProbeAnswer::Silent && !entry.unresolved.contains(&id) {
            entry.unresolved.push(id);
        }
        entry.driver.on_timeout(id);
    }
}

/// Deliver a legacy-key GET response into its owner's probe driver, then pump.
/// The response's contract id (`id`) was registered by [`fire_probe_get`]; the
/// FIRST of {this response, the watchdog} to remove the route owns the hop
/// (single-shot). Unknown ids are the race-loser and no-op.
pub(crate) async fn deliver_probe_response(id: ContractInstanceId, bytes: Vec<u8>) {
    // The per-fire token is deliberately IGNORED here (unlike the watchdog): a
    // GET response for `id` is a valid response for whatever fire currently owns
    // `id`'s route, and the driver is single-shot on its own outstanding
    // candidate — if the owning driver has already advanced past `id`, its
    // `on_response(id, ..)` is a no-op. So routing by the current owner is
    // always correct; only the stale-timeout case needs the token.
    let Some((owner_vk, _token)) = routes().remove(&id) else {
        // The route was already consumed — e.g. the timeout watchdog fired
        // first. Whichever ran first owns the outcome; the loser lands here.
        warn!("Probe GET response for {id} had no matching probe entry — ignoring");
        return;
    };
    {
        let mut drivers = drivers();
        if let Some(ProbeEntry { driver, .. }) = drivers.get_mut(&owner_vk) {
            // Observability only — the driver makes the actual decision. This
            // re-classifies the bytes purely to keep the pre-driver era's
            // per-candidate logs (once per probe hop, cost negligible).
            let ops = RiverProbeOps { owner_vk };
            match ops.decode(&bytes) {
                None => warn!(
                    "Legacy contract {id} returned undeserializable state — skipping generation"
                ),
                Some(state) if !ops.is_real(&state) => {
                    info!("Legacy contract {id} is empty — advancing to older generation")
                }
                Some(_) => {}
            }
            driver.on_response(id, &bytes);
        }
    }
    pump_probe(owner_vk).await;
}

/// Drive the probe for `owner_vk` one decision forward: ask the driver what to
/// do next, then either fire a legacy GET (arming a watchdog) or, when the
/// probe is finished, run the recover-or-seed side effects on its [`Outcome`].
async fn pump_probe(owner_vk: VerifyingKey) {
    let step = {
        let mut drivers = drivers();
        let Some(entry) = drivers.get_mut(&owner_vk) else {
            // Driver already finished and was removed — nothing to pump.
            return;
        };
        entry.driver.next_action()
    };

    match step {
        Step::Get(id) => fire_probe_get(owner_vk, id),
        Step::Done => {
            // Probe finished — drop the entry so a future probe for this owner
            // can start fresh, and so the dedup guard reads "not in flight".
            let Some(mut entry) = drivers().remove(&owner_vk) else {
                return;
            };
            let outcome = entry.driver.take_outcome();
            if let ProbePurpose::RekeyMerge { previous, seed } = entry.purpose {
                if let Some(outcome) = outcome {
                    finish_rekey_merge(owner_vk, previous, seed, outcome, &entry.unresolved);
                }
                return;
            }
            match outcome {
                Some(Outcome::Recovered { merged, source, .. }) => {
                    info!(
                        "Backward probe for room {:?} recovered real state from legacy contract \
                         {} — adopting and PUTting forward onto the current key",
                        MemberId::from(owner_vk),
                        source
                    );
                    adopt_recovered_probe_state(owner_vk, merged).await;
                }
                // Exhaustion (SeedLocal) and the no-legacy edge (NoLegacy, which
                // `start_backward_probe` guards against but the driver can still
                // produce) both PUT the device's local snapshot forward as the
                // genuine last resort — the only path on which local state is
                // seeded onto the network (the core design principle of #292).
                Some(Outcome::SeedLocal { local }) | Some(Outcome::NoLegacy { local })
                    if local.configuration.verify_signature(&owner_vk).is_err() =>
                {
                    // A placeholder (an imported room not yet synced) has no
                    // owner-signed configuration, so the contract would reject
                    // it. Leave the room `Subscribing`: the subscription sweep
                    // retries it, within the #290 bound.
                    info!(
                        "Backward probe for room {:?} found nothing and the local state is a \
                         placeholder — not seeding it",
                        MemberId::from(owner_vk)
                    );
                }
                Some(Outcome::SeedLocal { local }) | Some(Outcome::NoLegacy { local }) => {
                    info!(
                        "Backward probe for room {:?} exhausted — seeding current contract key \
                         with the local snapshot (last resort)",
                        MemberId::from(owner_vk)
                    );
                    seed_current_key_with_local(owner_vk, local).await;
                }
                None => {
                    // Outcome already taken (a duplicate pump) — nothing to do.
                }
            }
        }
    }
}

/// Apply a finished re-key merge (freenet/river#769): fold what the older keys
/// held into the room, clear the marker if the walk heard from every
/// generation, and, if asked, re-seed the current key.
fn finish_rekey_merge(
    owner_vk: VerifyingKey,
    previous: ContractInstanceId,
    seed: bool,
    outcome: Outcome<ChatRoomStateV1>,
    unresolved: &[ContractInstanceId],
) {
    let plan = plan_rekey_finish(outcome, unresolved);
    if plan.complete {
        info!(
            "Re-key merge for room {:?} finished ({})",
            MemberId::from(owner_vk),
            if plan.recovered.is_some() {
                "older generation merged"
            } else {
                "older generations hold nothing more"
            }
        );
    } else {
        warn!(
            "Re-key merge for room {:?}: {} older generation(s) did not answer — keeping \
             what this device has and retrying on the next load",
            MemberId::from(owner_vk),
            unresolved.len()
        );
    }
    let RekeyFinish {
        recovered,
        complete,
    } = plan;
    let had_recovered = recovered.is_some();
    crate::util::defer(move || {
        ROOMS.with_mut(|rooms| {
            let Some(room_data) = rooms.map.get_mut(&owner_vk) else {
                return;
            };
            // A merge that fails keeps the room as it was and keeps the
            // marker, so the next load tries again. Never a replace. The old
            // key's own upgrade pointer means nothing under the current key
            // (freenet/river#427), so it stays out of the room.
            let merged = match recovered {
                Some(state) => merge_recovered_into_room(
                    room_data,
                    crate::util::strip_upgrade_pointer(&state),
                    owner_vk,
                ),
                None => true,
            };
            if merged
                && complete
                && room_data
                    .previous_contract_key
                    .is_some_and(|k| *k.id() == previous)
            {
                room_data.previous_contract_key = None;
            }
        });
    });
    // Save the merged room and the cleared marker together (one room slot), and
    // let a subscribed room push the merged-in messages as a delta. FIFO
    // `defer` puts this after the merge above.
    if had_recovered || complete {
        crate::components::app::mark_needs_sync(owner_vk);
    }
    if seed {
        // After the merge (FIFO): the seed PUT reads the room from ROOMS, so it
        // carries the old key's messages and anything typed meanwhile.
        crate::util::defer(move || {
            SYNC_INFO.with_mut(|sync_info| sync_info.require_seed_put(&owner_vk));
            request_seed_put(owner_vk);
        });
    }
}

/// Register `id` as the outstanding probe GET for `owner_vk`, send the GET, and
/// arm a watchdog so an absent legacy generation cannot stall the probe.
fn fire_probe_get(owner_vk: VerifyingKey, id: ContractInstanceId) {
    // A fresh per-fire token: it stamps this route and is captured by the
    // watchdog below, so a stale watchdog from a completed probe cannot advance
    // a LATER probe that reused the same deterministic legacy id.
    let token = next_fire_token();

    // Register the route BEFORE sending the GET so the response handler can
    // resolve the legacy key when the reply arrives (and so `is_probe_instance`
    // routes it into the probe handler). Plain mutex — synchronous, no defer,
    // no signal re-entrancy.
    routes().insert(id, (owner_vk, token));

    // Keep the room out of the subscription-timeout sweep: a probe in flight IS
    // active subscription progress. Each hop refreshes `subscribing_since` (hops
    // are <= PROBE_GET_TIMEOUT apart, below REPUT_DELAY_MS — see the compile-time
    // assert above), so `rooms_awaiting_subscription` does not reclaim the room
    // mid-probe and re-issue a redundant GET / duplicate probe.
    // `touch_subscribing_since` only refreshes the timestamp — it does not force
    // the status — so it no-ops harmlessly if the room is absent from SYNC_INFO
    // or has genuinely already reached `Subscribed`.
    crate::util::defer(move || {
        SYNC_INFO.with_mut(|sync_info| {
            sync_info.touch_subscribing_since(&owner_vk);
        });
    });

    info!(
        "Backward probe for {:?}: GET legacy contract {}",
        MemberId::from(owner_vk),
        id
    );

    safe_spawn_local(async move {
        let get_request = ContractRequest::Get {
            key: id,
            // Legacy generations need their own WASM cached for the GET to
            // resolve on a node that has never seen that contract.
            return_contract_code: true,
            subscribe: false,
            blocking_subscribe: false,
        };
        let send_result = if let Some(web_api) = crate::components::app::WEB_API.write().as_mut() {
            web_api.send(ClientRequest::ContractOp(get_request)).await
        } else {
            Ok(()) // WebAPI gone — let the watchdog drive the probe forward.
        };
        if let Err(e) = send_result {
            warn!("Failed to send backward-probe GET for {id}: {e}");
            // Leave the route in place: the watchdog below treats the missing
            // response as a miss and advances the probe, so a transient send
            // failure still walks to the next generation rather than silently
            // abandoning recovery.
        }
    });

    // Watchdog: if no GET response consumes THIS route within PROBE_GET_TIMEOUT,
    // count the legacy generation as a miss and advance. `claim_route_if_current`
    // makes this single-shot AND cross-probe-safe: it advances only if `id`'s
    // route still carries THIS fire's `token`. Within one probe, whichever of
    // {response, watchdog} removes the route first owns the hop (the loser
    // finds it gone). Across probes, a completed probe's late watchdog finds a
    // later probe's fresh token on the reused id and no-ops — the guard the old
    // per-fire epoch counter used to provide.
    safe_spawn_local(async move {
        sleep(PROBE_GET_TIMEOUT).await;
        if claim_route_if_current(id, owner_vk, token) {
            warn!(
                "Backward-probe GET for {id} timed out after {}s — advancing past the \
                 legacy generation (its state is unknown)",
                PROBE_GET_TIMEOUT.as_secs()
            );
            // Silence, not absence: the walk moves on, but this generation's
            // state stays unknown.
            record_probe_answer(owner_vk, id, ProbeAnswer::Silent);
            // Box the recursive pump: `pump_probe` arms this watchdog, so an
            // un-boxed self-call would make the async fn's future infinitely
            // sized. The box breaks the type cycle.
            Box::pin(pump_probe(owner_vk)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};

    fn owner(seed: u8) -> (SigningKey, VerifyingKey) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let vk = sk.verifying_key();
        (sk, vk)
    }

    /// A room state with a genuine owner-signed configuration — what a legacy
    /// generation holding real state returns.
    fn signed_state(sk: &SigningKey) -> ChatRoomStateV1 {
        ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(Configuration::default(), sk),
            ..Default::default()
        }
    }

    fn encode(state: &ChatRoomStateV1) -> Vec<u8> {
        crate::util::to_cbor_vec(state)
    }

    fn id(n: u8) -> ContractInstanceId {
        ContractInstanceId::new([n; 32])
    }

    /// `RiverProbeOps` classification is exactly what the deleted
    /// `handle_probe_get_response` applied inline: a default/placeholder state
    /// is NOT real (its signature does not verify → a miss), an owner-signed
    /// state IS real (a hit), and undecodable bytes decode to `None` (a miss so
    /// the probe advances rather than panicking).
    #[test]
    fn river_probe_ops_classifies_real_decodable_and_garbage() {
        let (sk, vk) = owner(1);
        let ops = RiverProbeOps { owner_vk: vk };

        let real = signed_state(&sk);
        assert!(
            ops.is_real(&real),
            "owner-signed configuration must be real"
        );

        let empty = ChatRoomStateV1::default();
        assert!(
            !ops.is_real(&empty),
            "default state must NOT be real (a miss)"
        );

        assert!(
            ops.decode(&encode(&real)).is_some(),
            "valid CBOR must decode"
        );
        assert!(
            ops.decode(&[0xffu8; 8]).is_none(),
            "undecodable bytes must map to None (a miss, never a panic)"
        );
    }

    /// `prepare_forward` MUST be identity. River strips the upgrade pointer only
    /// in `put_state_to_current_key` (freenet/river#427) and adopts the
    /// UNSTRIPPED merged state locally; if `RiverProbeOps` ever stripped in
    /// `prepare_forward`, the locally-adopted state would silently differ from
    /// the shipped behavior. This pins that the pointer survives `prepare_forward`.
    #[test]
    fn river_probe_ops_prepare_forward_keeps_upgrade_pointer() {
        use river_core::room_state::upgrade::{AuthorizedUpgradeV1, OptionalUpgradeV1, UpgradeV1};

        let (sk, vk) = owner(2);
        let ops = RiverProbeOps { owner_vk: vk };

        let upgrade = UpgradeV1 {
            owner_member_id: MemberId::from(&vk),
            version: 1,
            new_chatroom_address: blake3::Hash::from([7u8; 32]),
        };
        let authorized = AuthorizedUpgradeV1::new(upgrade, &sk);
        let mut state = signed_state(&sk);
        state.upgrade = OptionalUpgradeV1(Some(authorized));

        let forwarded = ops.prepare_forward(state.clone());
        assert_eq!(
            forwarded.upgrade, state.upgrade,
            "prepare_forward must NOT strip the upgrade pointer — stripping happens only in \
             put_state_to_current_key (freenet/river#427)"
        );
    }

    /// `merge_with_local` (River's `merge_room_states`) preserves the recovered
    /// state's owner-signed configuration when folding in an empty local
    /// snapshot (the fresh-import case), so the merged state stays valid.
    #[test]
    fn river_probe_ops_merge_preserves_recovered_configuration() {
        let (sk, vk) = owner(3);
        let ops = RiverProbeOps { owner_vk: vk };
        let recovered = signed_state(&sk);
        let merged = ops.merge_with_local(recovered, &ChatRoomStateV1::default());
        assert!(
            merged.configuration.verify_signature(&vk).is_ok(),
            "merge must preserve the recovered owner-signed configuration"
        );
    }

    /// End-to-end at the River level: two real generations, the driver adopts
    /// the NEWEST and never reads the older (anti-rollback). Wiring
    /// `RiverProbeOps` into a real `ProbeDriver` catches a regression in the
    /// decode/is_real classification, not just the driver's own generic logic.
    #[test]
    fn driver_adopts_newest_real_generation_with_river_ops() {
        let (sk, vk) = owner(4);
        let mut driver = ProbeDriver::new(
            RiverProbeOps { owner_vk: vk },
            ChatRoomStateV1::default(),
            NewestFirst::assume_ordered(vec![id(9), id(5)]),
            SelectionPolicy::NewestFirstWins,
        );
        let Step::Get(newest) = driver.next_action() else {
            panic!("expected a GET")
        };
        assert_eq!(newest, id(9), "the newest generation must be probed first");
        driver.on_response(newest, &encode(&signed_state(&sk)));
        assert_eq!(driver.next_action(), Step::Done);
        let Some(Outcome::Recovered { source, merged, .. }) = driver.take_outcome() else {
            panic!("expected recovery from the newest generation");
        };
        assert_eq!(
            source,
            id(9),
            "an older generation must not shadow the newer"
        );
        assert!(merged.configuration.verify_signature(&vk).is_ok());
    }

    /// End-to-end: every generation misses (a timeout then an empty state), so
    /// the driver seeds the device's local snapshot forward — the
    /// no-silent-data-loss guarantee on exhaustion.
    #[test]
    fn driver_exhausts_to_seed_local_with_river_ops() {
        let (sk, vk) = owner(5);
        let local = signed_state(&sk);
        let mut driver = ProbeDriver::new(
            RiverProbeOps { owner_vk: vk },
            local,
            NewestFirst::assume_ordered(vec![id(2), id(1)]),
            SelectionPolicy::NewestFirstWins,
        );
        let Step::Get(a) = driver.next_action() else {
            panic!()
        };
        driver.on_timeout(a); // absent generation (watchdog)
        let Step::Get(b) = driver.next_action() else {
            panic!()
        };
        driver.on_response(b, &encode(&ChatRoomStateV1::default())); // empty → miss
        assert_eq!(driver.next_action(), Step::Done);
        let Some(Outcome::SeedLocal { local: seeded }) = driver.take_outcome() else {
            panic!("exhaustion must seed the local snapshot");
        };
        assert!(
            seeded.configuration.verify_signature(&vk).is_ok(),
            "the seeded local snapshot must survive exhaustion intact"
        );
    }

    /// Single-shot correlation WITHIN one probe: a candidate times out (the
    /// probe advances), then its real response arrives late — it must be
    /// ignored, not adopted. This is the driver's per-candidate correlation;
    /// the CROSS-probe same-owner case is covered separately by the route
    /// token (see `stale_watchdog_does_not_advance_a_later_same_owner_probe`).
    #[test]
    fn driver_ignores_late_response_after_timeout_with_river_ops() {
        let (sk, vk) = owner(6);
        let mut driver = ProbeDriver::new(
            RiverProbeOps { owner_vk: vk },
            ChatRoomStateV1::default(),
            NewestFirst::assume_ordered(vec![id(2), id(1)]),
            SelectionPolicy::NewestFirstWins,
        );
        let Step::Get(a) = driver.next_action() else {
            panic!()
        };
        driver.on_timeout(a);
        let Step::Get(b) = driver.next_action() else {
            panic!()
        };
        // Late (stale) hit for the timed-out candidate a — must be dropped.
        driver.on_response(a, &encode(&signed_state(&sk)));
        assert_eq!(
            driver.next_action(),
            Step::Get(b),
            "a late response for an advanced-past candidate must not adopt it"
        );
        driver.on_response(b, &encode(&ChatRoomStateV1::default()));
        assert_eq!(driver.next_action(), Step::Done);
        assert!(
            matches!(driver.take_outcome(), Some(Outcome::SeedLocal { .. })),
            "the late hit must not have been adopted"
        );
    }

    /// Regression (Codex review of #398 phase 2): a stale watchdog from a
    /// COMPLETED probe must not advance a LATER probe for the same owner that
    /// re-registered the same deterministic legacy id.
    ///
    /// Sequential same-owner probes walk the SAME legacy ids (they are derived
    /// from the owner + a fixed legacy-WASM registry), so probe 1's still-armed
    /// watchdog and probe 2's fresh route collide on one id. The per-fire route
    /// token is what tells them apart: the stale watchdog's token no longer
    /// matches, so `claim_route_if_current` refuses to claim probe 2's route,
    /// and probe 2 stays on its NEWEST candidate. Without the token (the bug the
    /// deleted epoch counter guarded), the stale watchdog would skip probe 2's
    /// newest generation and roll it back to an older one.
    #[test]
    fn stale_watchdog_does_not_advance_a_later_same_owner_probe() {
        let (_sk, vk) = owner(7);
        // Distinct ids so this test can't collide with others sharing the
        // process-global PROBE_ROUTES / PROBE_DRIVERS maps under parallel
        // `cargo test`.
        let newest = id(70);
        let older = id(71);

        // This exercises the ROUTE + WATCHDOG-GATE layer where the bug actually
        // lives — PROBE_ROUTES, the per-fire token, `claim_route_if_current`,
        // and a REAL driver in PROBE_DRIVERS — not just an in-memory driver. It
        // models the effects of `fire_probe_get` / `deliver_probe_response` /
        // the watchdog on the shared maps directly, because those fns are async
        // and spawn tasks / touch Dioxus signals that cannot run in a host unit
        // test.

        // --- Probe 1: fire `newest` (token t1), then complete the hop. ---
        // `fire_probe_get` inserts (owner, token) and arms a watchdog W1 that
        // holds t1.
        let t1 = next_fire_token();
        routes().insert(newest, (vk, t1));
        // A real response arrives: `deliver_probe_response` removes the route
        // and probe 1 completes. W1 stays armed with t1 — nothing cancels it.
        assert_eq!(routes().remove(&newest), Some((vk, t1)));

        // --- Probe 2: same owner, re-fires the SAME `newest` id (fresh token
        // t2); its driver is live in PROBE_DRIVERS, sitting on `newest`. ---
        let t2 = next_fire_token();
        routes().insert(newest, (vk, t2));
        let mut probe2 = ProbeDriver::new(
            RiverProbeOps { owner_vk: vk },
            ChatRoomStateV1::default(),
            NewestFirst::assume_ordered(vec![newest, older]),
            SelectionPolicy::NewestFirstWins,
        );
        assert_eq!(
            probe2.next_action(),
            Step::Get(newest),
            "probe 2 starts on its newest candidate"
        );
        drivers().insert(
            vk,
            ProbeEntry {
                driver: probe2,
                unresolved: Vec::new(),
                purpose: ProbePurpose::Recover,
            },
        );

        // --- W1 fires LATE. Run the watchdog's EXACT gate: it advances
        // (on_timeout + pump) ONLY if `claim_route_if_current` returns true. ---
        let w1_claimed = claim_route_if_current(newest, vk, t1);
        if w1_claimed {
            // The buggy path this guards: this would advance probe 2 past its
            // newest candidate onto `older` — a silent rollback.
            drivers().get_mut(&vk).unwrap().driver.on_timeout(newest);
        }

        assert!(
            !w1_claimed,
            "a stale watchdog (old token t1) must not claim probe 2's reused route"
        );
        assert_eq!(
            routes().get(&newest),
            Some(&(vk, t2)),
            "probe 2's route must survive the stale watchdog untouched"
        );
        assert_eq!(
            drivers().get_mut(&vk).unwrap().driver.next_action(),
            Step::Get(newest),
            "probe 2's driver must be untouched — still on its newest candidate, no rollback"
        );

        // --- Contrast: probe 2's OWN watchdog (token t2) DOES claim its route. ---
        assert!(
            claim_route_if_current(newest, vk, t2),
            "the current fire's watchdog claims its own route"
        );

        // Cleanup the process-global maps this test populated.
        drivers().remove(&vk);
        routes().remove(&newest);
    }

    // ---- freenet/river#769: re-key merge -------------------------------------

    /// An owner-signed room holding `bodies` as owner-authored messages, one
    /// second apart, so a merge has a stable order to keep.
    fn room_with_messages(sk: &SigningKey, bodies: &[&str]) -> ChatRoomStateV1 {
        use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
        use std::time::{Duration, SystemTime};
        let vk = sk.verifying_key();
        let owner_id = MemberId::from(&vk);
        let mut state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    ..Configuration::default()
                },
                sk,
            ),
            ..Default::default()
        };
        // Fixed, so the same body is the same message in every fixture.
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        for body in bodies {
            let n: u64 = body.trim_start_matches('m').parse().unwrap();
            let msg = MessageV1 {
                room_owner: owner_id,
                author: owner_id,
                content: RoomMessageBody::public(body.to_string()),
                time: base + Duration::from_secs(n),
            };
            state
                .recent_messages
                .messages
                .push(AuthorizedMessageV1::new(msg, sk));
        }
        state
    }

    fn bodies(n: std::ops::RangeInclusive<u32>) -> Vec<String> {
        n.map(|i| format!("m{i}")).collect()
    }

    fn texts(state: &ChatRoomStateV1) -> Vec<String> {
        let mut out: Vec<String> = state
            .recent_messages
            .messages
            .iter()
            .map(|m| m.message.content.as_public_string().unwrap_or_default())
            .collect();
        out.sort();
        out
    }

    fn room_data(owner_sk: &SigningKey, state: ChatRoomStateV1) -> crate::room_data::RoomData {
        use crate::constants::ROOM_CONTRACT_WASM;
        use freenet_stdlib::prelude::{ContractCode, ContractKey, Parameters};
        let owner_vk = owner_sk.verifying_key();
        let params = crate::util::to_cbor_vec(&ChatRoomParametersV1 { owner: owner_vk });
        crate::room_data::RoomData {
            owner_vk,
            room_state: state,
            self_sk: Some(owner_sk.clone()),
            self_vk: None,
            contract_key: ContractKey::from_params_and_code(
                Parameters::from(params),
                &ContractCode::from(ROOM_CONTRACT_WASM),
            ),
            last_read_message_id: None,
            secrets: HashMap::new(),
            current_secret_version: None,
            last_secret_rotation: None,
            key_migrated_to_delegate: false,
            self_authorized_member: None,
            invite_chain: vec![],
            self_member_info: None,
            self_nickname: None,
            previous_contract_key: None,
            invitation_secrets: std::collections::BTreeMap::new(),
        }
    }

    /// Run a re-key merge walk to completion through the REAL adapter state
    /// (the shared `PROBE_DRIVERS` entry and `record_probe_answer`, the same
    /// calls the NotFound handler and the watchdog make), answering each GET
    /// from `answers` in order. Returns what the merge would apply.
    fn walk_rekey(
        owner_sk: &SigningKey,
        local: ChatRoomStateV1,
        candidates: Vec<ContractInstanceId>,
        answers: Vec<Option<Result<ChatRoomStateV1, ProbeAnswer>>>,
    ) -> RekeyFinish {
        let vk = owner_sk.verifying_key();
        let previous = *candidates.last().unwrap();
        drivers().insert(
            vk,
            ProbeEntry {
                driver: ProbeDriver::new(
                    RiverProbeOps { owner_vk: vk },
                    local,
                    NewestFirst::assume_ordered(candidates.clone()),
                    SelectionPolicy::NewestFirstWins,
                ),
                unresolved: Vec::new(),
                purpose: ProbePurpose::RekeyMerge {
                    previous,
                    seed: true,
                },
            },
        );
        let mut answers = answers.into_iter();
        loop {
            let step = drivers().get_mut(&vk).unwrap().driver.next_action();
            match step {
                Step::Get(id) => match answers.next().expect("an answer per GET") {
                    Some(Ok(state)) => drivers()
                        .get_mut(&vk)
                        .unwrap()
                        .driver
                        .on_response(id, &encode(&state)),
                    Some(Err(answer)) => record_probe_answer(vk, id, answer),
                    None => panic!("walk asked for {id}, which the test did not expect"),
                },
                Step::Done => break,
            }
        }
        let mut entry = drivers().remove(&vk).unwrap();
        assert_eq!(
            entry.purpose,
            ProbePurpose::RekeyMerge {
                previous,
                seed: true
            }
        );
        plan_rekey_finish(entry.driver.take_outcome().unwrap(), &entry.unresolved)
    }

    /// Only the generations from the newest legacy one down to the one the
    /// device last synced under are asked, newest first; an unregistered
    /// generation is asked on its own.
    #[test]
    fn rekey_candidates_stop_at_the_generation_last_synced() {
        let legacy = vec![id(30), id(29), id(28), id(27)];
        assert_eq!(rekey_candidates(&legacy, id(30)), vec![id(30)]);
        assert_eq!(
            rekey_candidates(&legacy, id(28)),
            vec![id(30), id(29), id(28)],
            "skipped releases are asked newest first, down to the device's own"
        );
        assert_eq!(rekey_candidates(&legacy, id(99)), vec![id(99)]);
        assert_eq!(rekey_candidates(&[], id(5)), vec![id(5)]);
    }

    /// The rehearsal's failure: 16 messages on the old key, 10 in this
    /// device's snapshot. Every one of the 16 must reach the state the seed PUT
    /// carries, and the walk counts as finished.
    #[test]
    fn rekey_merge_brings_over_messages_only_on_the_old_key() {
        let sk = SigningKey::from_bytes(&[81u8; 32]);
        let old_key: Vec<String> = bodies(1..=16);
        let local: Vec<String> = bodies(1..=10);
        let old_refs: Vec<&str> = old_key.iter().map(String::as_str).collect();
        let local_refs: Vec<&str> = local.iter().map(String::as_str).collect();
        let local_state = room_with_messages(&sk, &local_refs);

        let finish = walk_rekey(
            &sk,
            local_state.clone(),
            vec![id(81)],
            vec![Some(Ok(room_with_messages(&sk, &old_refs)))],
        );
        assert!(finish.complete, "the old key answered: the merge is done");
        let recovered = finish.recovered.expect("the old key's state is merged");

        let mut room = room_data(&sk, local_state);
        assert!(merge_recovered_into_room(
            &mut room,
            crate::util::strip_upgrade_pointer(&recovered),
            sk.verifying_key()
        ));
        let mut expected = old_key.clone();
        expected.sort();
        assert_eq!(texts(&room.room_state), expected, "all 16 messages survive");
    }

    /// A member who upgraded first already seeded the new key, and this device
    /// has merged that in, plus messages of its own. The old key's state merges
    /// INTO that; nothing already in the room is dropped.
    #[test]
    fn rekey_merge_never_clobbers_what_the_room_already_holds() {
        let sk = SigningKey::from_bytes(&[82u8; 32]);
        // ROOMS: what another member seeded (m1..m5) plus this device's m20.
        let room_state = room_with_messages(&sk, &["m1", "m2", "m3", "m4", "m5", "m20"]);
        // The old key also has m6..m8, which nobody carried forward.
        let old = room_with_messages(&sk, &["m1", "m2", "m3", "m6", "m7", "m8"]);
        let mut room = room_data(&sk, room_state);
        assert!(merge_recovered_into_room(
            &mut room,
            old,
            sk.verifying_key()
        ));
        let mut expected: Vec<String> = ["m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "m20"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        expected.sort();
        assert_eq!(texts(&room.room_state), expected);
    }

    /// The old key never answers: nothing is merged, the local state is left
    /// as it is (it seeds the new key on its own), and the walk is NOT finished,
    /// so the marker stays for the next load. A typed NotFound, by contrast, is
    /// an answer.
    #[test]
    fn rekey_merge_with_an_unreachable_old_key_keeps_local_and_retries() {
        let sk = SigningKey::from_bytes(&[83u8; 32]);
        let local = room_with_messages(&sk, &["m1", "m2"]);

        let silent = walk_rekey(
            &sk,
            local.clone(),
            vec![id(83)],
            vec![Some(Err(ProbeAnswer::Silent))],
        );
        assert!(
            silent.recovered.is_none(),
            "no state arrived, none is merged"
        );
        assert!(
            !silent.complete,
            "silence is not absence: the merge must be retried, so the marker stays"
        );

        let absent = walk_rekey(
            &sk,
            local,
            vec![id(84)],
            vec![Some(Err(ProbeAnswer::Absent))],
        );
        assert!(absent.recovered.is_none());
        assert!(
            absent.complete,
            "a NotFound answered: the old key holds nothing more"
        );
    }

    /// A device that skipped releases: the two newer generations it never ran
    /// answer NotFound, and the one it last synced under holds the messages.
    #[test]
    fn rekey_merge_walks_down_skipped_generations() {
        let sk = SigningKey::from_bytes(&[85u8; 32]);
        let local = room_with_messages(&sk, &["m1"]);
        let legacy = vec![id(90), id(89), id(88), id(87)];
        let candidates = rekey_candidates(&legacy, id(88));
        assert_eq!(candidates, vec![id(90), id(89), id(88)]);

        let finish = walk_rekey(
            &sk,
            local.clone(),
            candidates.clone(),
            vec![
                Some(Err(ProbeAnswer::Absent)),
                Some(Err(ProbeAnswer::Absent)),
                Some(Ok(room_with_messages(&sk, &["m1", "m2", "m3"]))),
            ],
        );
        assert!(finish.complete);
        assert_eq!(
            texts(&finish.recovered.expect("the device's own generation hit")),
            vec!["m1", "m2", "m3"]
        );

        // The newest generation never answered: what was found is merged
        // (a merge cannot lose anything), but the walk is not finished.
        let finish = walk_rekey(
            &sk,
            local,
            candidates,
            vec![
                Some(Err(ProbeAnswer::Silent)),
                Some(Err(ProbeAnswer::Absent)),
                Some(Ok(room_with_messages(&sk, &["m1", "m2", "m3"]))),
            ],
        );
        assert!(finish.recovered.is_some());
        assert!(
            !finish.complete,
            "a newer generation that never answered keeps the merge open"
        );
    }

    /// A walk already running in the background for an owner is told to seed
    /// when that room's current key turns out to be empty, rather than a
    /// second walk starting or the room going unseeded.
    #[test]
    fn an_empty_current_key_makes_a_background_merge_seed() {
        let sk = SigningKey::from_bytes(&[86u8; 32]);
        let vk = sk.verifying_key();
        drivers().insert(
            vk,
            ProbeEntry {
                driver: ProbeDriver::new(
                    RiverProbeOps { owner_vk: vk },
                    ChatRoomStateV1::default(),
                    NewestFirst::assume_ordered(vec![id(86)]),
                    SelectionPolicy::NewestFirstWins,
                ),
                unresolved: Vec::new(),
                purpose: ProbePurpose::RekeyMerge {
                    previous: id(86),
                    seed: false,
                },
            },
        );
        assert!(futures::executor::block_on(start_backward_probe(
            vk,
            ChatRoomStateV1::default()
        )));
        assert_eq!(
            drivers().get(&vk).unwrap().purpose,
            ProbePurpose::RekeyMerge {
                previous: id(86),
                seed: true
            }
        );
        drivers().remove(&vk);
    }

    /// freenet/river#769 wiring, source pin: `finish_rekey_merge` acts on the
    /// plan through ROOMS, SYNC_INFO and the synchronizer, which no native test
    /// can drive (the decisions themselves are tested above). The marker is
    /// cleared only after a merge that landed AND heard from every generation,
    /// and the seed is scheduled after the merge, so the PUT carries it.
    #[test]
    fn finish_rekey_merge_clears_after_a_complete_merge_and_seeds_last() {
        let src: String = crate::util::strip_comments(
            include_str!("backward_probe.rs")
                .split("mod tests {")
                .next()
                .unwrap(),
        )
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
        let start = src.find("fnfinish_rekey_merge(").expect("fn exists");
        let body = &src[start..];
        let body = &body[..body.find("fnfire_probe_get(").expect("next fn")];
        assert!(
            body.contains(
                "ifmerged&&complete&&room_data.previous_contract_key.is_some_and(|k|*k.id()==previous){room_data.previous_contract_key=None;}"
            ),
            "clear the marker only after a merge that landed and a complete walk"
        );
        let merge = body
            .find("crate::util::defer(move||{ROOMS.with_mut(")
            .expect("the merge runs in a deferred ROOMS write");
        let seed = body.find("ifseed{").expect("the seed is conditional");
        assert!(merge < seed, "the seed is scheduled after the merge (FIFO)");
        let seed_arm = &body[seed..];
        let flag = seed_arm
            .find("sync_info.require_seed_put(&owner_vk)")
            .expect("the seed must force the PUT route");
        let put = seed_arm
            .find("request_seed_put(owner_vk)")
            .expect("the seed must hand the room to process_rooms");
        assert!(
            flag < put,
            "PUT route forced before the room is handed back"
        );
        assert!(
            seed_arm.contains("crate::util::defer(move||{SYNC_INFO"),
            "the seed itself is deferred, behind the merge"
        );
    }
}
