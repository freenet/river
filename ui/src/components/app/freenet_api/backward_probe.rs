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
//! A typed `NotFound` for a probe GET advances the walk at once instead of
//! waiting out the watchdog. It is not proof the generation is empty: on
//! Freenet a `NotFound` is routinely a routing dead-end. The #292 recovery
//! treats it as a miss, as it always treated a timeout; a re-key merge never
//! takes it as grounds to stop retrying (below).
//!
//! ## Re-key merge (freenet/river#769)
//!
//! When the bundled room-contract WASM changes, every room's key moves and the
//! new key starts empty. The device's local snapshot is not the whole room:
//! messages that reached the OLD key after this device last saved exist only
//! there. So for a room whose stored contract key is from an older generation,
//! [`start_rekey_merge`] walks the generations from the newest legacy one down
//! to the one this device last synced under, folds every one that has state,
//! merges the result into `ROOMS` (CRDT merge, never a replace), and only then
//! lets the room's seed PUT go out, carrying the merged state. The room's
//! `rekey_merge.owed` is the durable "merge still owed" marker. It is cleared
//! only once the generation it names has answered with the room's state and
//! that state has merged in, so an old key that is slow, unreachable or
//! answers `NotFound` is asked again on a later load (only the owed keys, in
//! the background), until [`MAX_UNFINISHED_REKEY_LOADS`] loads and
//! [`REKEY_GIVE_UP_AFTER`] have both passed.
//!
//! The walk holds the seed PUT for at most [`REKEY_SEED_WAIT`]: dead older
//! keys each wait out the 12 s watchdog, and a room must not sit at
//! "connecting" for that. At the deadline the room is seeded with what it
//! holds and the walk finishes in the background; its merge then reaches the
//! new key as an ordinary update.

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
    FoldAllAck, NewestFirst, Outcome, ProbeDriver, ProbeStateOps, RollbackRiskAck, SelectionPolicy,
    Step, DEFAULT_MAX_PROBE_HOPS,
};
use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
use freenet_stdlib::prelude::ContractInstanceId;
use river_core::room_state::member::MemberId;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
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

/// How long a re-key merge may hold a room's seed PUT (freenet/river#769).
/// The immediate predecessor generation normally answers well inside this
/// (0.2 s in the rehearsal), so the seed carries the old key's messages. A walk
/// still running at the deadline (dead older keys, each waiting out the 12 s
/// watchdog) seeds what the room holds now and finishes in the background;
/// what it merges later reaches the new key as an ordinary update.
const REKEY_SEED_WAIT: Duration = Duration::from_secs(5);

/// How many consecutive page loads a re-key merge may end with keys still owed
/// before the remaining keys are given up. Each such key held this room, but
/// one that has not returned it in this many loads is almost certainly gone
/// from the network, and asking again costs a GET on every load.
pub(crate) const MAX_UNFINISHED_REKEY_LOADS: u32 = 5;

/// And only once this long has passed since the room first owed the merge:
/// counting loads alone, tabs, reloads and loads before the node has peers
/// could give up a live key holding the only copy of some messages within
/// minutes.
pub(crate) const REKEY_GIVE_UP_AFTER: Duration = Duration::from_secs(3 * 24 * 60 * 60);

/// River's app-supplied state semantics for the sans-IO decision driver: how to
/// decode a legacy GET response, whether it is *real* state, and how to fold it
/// with the device's local snapshot. Everything else (candidate order,
/// single-shot correlation, exhaustion) is owned by the driver.
struct RiverProbeOps {
    /// The owner whose stranded room this probe is recovering.
    owner_vk: VerifyingKey,
    /// Set if folding two generations together failed, so the fold dropped
    /// one of them (re-key merges fold every generation, freenet/river#769).
    fold_failed: Arc<AtomicBool>,
}

impl RiverProbeOps {
    fn new(owner_vk: VerifyingKey) -> Self {
        Self {
            owner_vk,
            fold_failed: Arc::new(AtomicBool::new(false)),
        }
    }
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

    fn merge_generations(&self, newer: ChatRoomStateV1, older: ChatRoomStateV1) -> ChatRoomStateV1 {
        // Only a re-key merge folds (`SelectionPolicy::FoldAll`). CRDT-merge
        // the older generation in; if that fails, keep the newer and say so,
        // so the walk is not recorded as finished on a fold that lost a side.
        let params = ChatRoomParametersV1 {
            owner: self.owner_vk,
        };
        let mut merged = newer.clone();
        match merged.merge(&newer, &params, &older) {
            Ok(()) => merged,
            Err(e) => {
                warn!("Re-key merge: folding an older generation failed ({e}); keeping the newer");
                self.fold_failed.store(true, Ordering::Relaxed);
                newer
            }
        }
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
/// ([`take_probe_response`]); the token lets a watchdog tell its own outstanding
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
    /// freenet/river#769: the room owes merges from older generations it
    /// was synced under ([`ProbeEntry::owed`]). Merge what the older keys hold
    /// into `ROOMS`; then, if `seed`, re-seed the current key through the
    /// ordinary seed-PUT path, so the PUT carries the merged state.
    RekeyMerge { seed: bool },
}

/// One owner's in-flight probe.
struct ProbeEntry {
    driver: ProbeDriver<RiverProbeOps>,
    /// Candidates that answered with real state. A re-key merge is recorded
    /// as finished only once the generation it owes a merge from is among
    /// them: a `NotFound`, or silence, from that key is never proof it is
    /// empty, since this device synced the room under it.
    hits: Vec<ContractInstanceId>,
    /// Shared with the driver's [`RiverProbeOps`].
    fold_failed: Arc<AtomicBool>,
    /// For a re-key merge, the generations the room owes a merge from.
    owed: Vec<ContractInstanceId>,
    purpose: ProbePurpose,
    /// Unique per walk, so a seed deadline armed for one walk cannot act on a
    /// later walk for the same owner.
    walk: u64,
}

/// Build a backward-probe driver, in ONE place, so production and every test
/// run the same configuration (riverctl does the same:
/// `cli/src/api.rs::new_probe_driver`).
///
/// `continue_past_unknown` (freenet-migrate 0.6) on `NewestFirstWins`: a typed
/// `NotFound` reaches the driver as `on_absent` ([`take_probe_absent`]), but a
/// GET that times out is `on_unknown`, and 0.6 made `NewestFirstWins` STOP at
/// the first unknown. A room lives on ONE of the registry's generations, so
/// stopping at the first silent one would end the probe before it reached the
/// generation holding the room. The rollback the ack names is the one the 0.5
/// driver already accepted (it walked on past every timeout);
/// `NewestFirstWins` still stops at the first generation with real state.
/// `FoldAll` continues past an unknown anyway.
fn new_probe_driver(
    ops: RiverProbeOps,
    local: ChatRoomStateV1,
    candidates_newest_first: Vec<ContractInstanceId>,
    policy: SelectionPolicy,
) -> ProbeDriver<RiverProbeOps> {
    let walk_past_silence = matches!(policy, SelectionPolicy::NewestFirstWins);
    let driver = ProbeDriver::new(
        ops,
        local,
        NewestFirst::assume_ordered(candidates_newest_first),
        policy,
    );
    if walk_past_silence {
        driver.continue_past_unknown(
            RollbackRiskAck::i_understand_continuing_past_silence_can_roll_back(),
        )
    } else {
        driver
    }
}

/// What a finished #292 recovery probe does with its [`Outcome`].
#[derive(Debug)]
enum ProbeFinish {
    /// A legacy generation had real state: adopt `merged` and PUT it forward.
    Adopt(ChatRoomStateV1, ContractInstanceId),
    /// Nothing was recovered: seed the current key with the local snapshot.
    SeedLocal(ChatRoomStateV1),
    /// An outcome this build has not reasoned about, or one already taken: do
    /// nothing (the next load probes again).
    Nothing,
}

/// Map a finished recovery probe's [`Outcome`] to what River does with it.
/// Pure, so the 0.6 `Indeterminate` arm is unit-testable.
///
/// `Indeterminate` (freenet-migrate 0.6) is "at least one candidate never
/// answered" and nothing real was found. The 0.5 driver ended the same answers
/// in `SeedLocal`, so it seeds the local snapshot just the same. Not a seal:
/// the seed is an idempotent PUT of the device's own snapshot, and a later
/// probe that finds real state still adopts it.
fn probe_finish(outcome: Option<Outcome<ChatRoomStateV1>>) -> ProbeFinish {
    match outcome {
        Some(Outcome::Recovered { merged, source, .. }) => ProbeFinish::Adopt(merged, source),
        Some(Outcome::SeedLocal { local })
        | Some(Outcome::NoLegacy { local })
        | Some(Outcome::Indeterminate { local, .. }) => ProbeFinish::SeedLocal(local),
        // `Outcome` is `#[non_exhaustive]`: an arm this build has not reasoned
        // about must not adopt or seed anything.
        Some(_) | None => ProbeFinish::Nothing,
    }
}

impl ProbeEntry {
    fn new(
        owner_vk: VerifyingKey,
        local: ChatRoomStateV1,
        candidates: Vec<ContractInstanceId>,
        purpose: ProbePurpose,
        owed: Vec<ContractInstanceId>,
    ) -> Self {
        let ops = RiverProbeOps::new(owner_vk);
        let fold_failed = ops.fold_failed.clone();
        // A re-key merge folds every generation it asks into the room: the
        // result is CRDT-merged into ROOMS, never adopted in place of it, so
        // the anti-rollback reason for stopping at the newest hit does not
        // apply, and stopping there would skip the very generation this device
        // last synced under. River's merge re-applies the caps and the
        // membership/ban cleanup, so an older generation's messages cannot
        // outlive them. The #292 recovery adopts exactly one generation.
        let policy = match purpose {
            ProbePurpose::Recover => SelectionPolicy::NewestFirstWins,
            ProbePurpose::RekeyMerge { .. } => SelectionPolicy::FoldAll(
                FoldAllAck::i_understand_fold_all_resurrects_without_tombstones(),
            ),
        };
        // Candidates are newest-first by construction (see the callers).
        let driver = new_probe_driver(ops, local, candidates, policy);
        Self {
            driver,
            hits: Vec::new(),
            fold_failed,
            owed,
            purpose,
            walk: next_fire_token(),
        }
    }

    /// Blocking probes hold their room out of the subscription sweep; a
    /// background re-key merge must not, or it would also stretch the timeout
    /// of the room's own subscribe GET.
    fn holds_subscription(&self) -> bool {
        !matches!(self.purpose, ProbePurpose::RekeyMerge { seed: false, .. })
    }
}

/// Legacy ids whose probe GET the watchdog gave up on while it may still be
/// in flight at the node, with how many such fires are outstanding. A
/// `NotFound` carries no fire token, so one arriving for such an id may belong
/// to the abandoned fire rather than a later probe that re-registered the id:
/// it is consumed here and ignored, and the later fire falls back to its own
/// watchdog (slower, never wrong).
static ORPHANED_FIRES: LazyLock<Mutex<HashMap<ContractInstanceId, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn orphaned_fires() -> MutexGuard<'static, HashMap<ContractInstanceId, u32>> {
    ORPHANED_FIRES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Fire tokens whose GET was never sent (no WebAPI, or the send failed), so
/// their watchdog does not count them as possibly in flight.
static UNSENT_FIRES: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn unsent_fires() -> MutexGuard<'static, HashSet<u64>> {
    UNSENT_FIRES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Consume one orphaned fire for `id`, if any. Returns whether one was.
fn take_orphaned_fire(id: &ContractInstanceId) -> bool {
    let mut orphans = orphaned_fires();
    match orphans.get_mut(id) {
        Some(n) if *n > 1 => {
            *n -= 1;
            true
        }
        Some(_) => {
            orphans.remove(id);
            true
        }
        None => false,
    }
}

/// One in-flight probe per owner. The driver owns the sans-IO probe decision
/// state; this module only pumps I/O through it. Deduplicating concurrent
/// probes for one owner is our job (see [`start_backward_probe`]).
static PROBE_DRIVERS: LazyLock<Mutex<HashMap<VerifyingKey, ProbeEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Owners a re-key merge has been started for on this page load. A merge
/// runs at most once per load; one that could not finish is retried on the
/// NEXT load, from the persisted marker.
static REKEY_ATTEMPTED: LazyLock<Mutex<HashSet<VerifyingKey>>> =
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
fn rekey_attempted() -> MutexGuard<'static, HashSet<VerifyingKey>> {
    REKEY_ATTEMPTED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The generations a re-key merge asks, newest first: every legacy generation
/// from the newest down to and including the oldest one the room owes a merge
/// from (freenet/river#769).
///
/// Nothing older is asked. This device's snapshot already holds what it saw
/// under the keys it synced under, so an older key can only add what was
/// written there before this device moved on, which is the #292 dormant-room
/// recovery's job, not a re-key's. Bounding the list is what keeps the seed
/// PUT prompt: one GET in the ordinary case, one per skipped release otherwise.
///
/// An owed key missing from the registry (a development build, say) is asked
/// too, last: it is a key known to hold this device's room.
pub(crate) fn rekey_candidates(
    legacy_newest_first: &[ContractInstanceId],
    owed: &[ContractInstanceId],
) -> Vec<ContractInstanceId> {
    let deepest = owed
        .iter()
        .filter_map(|o| legacy_newest_first.iter().position(|id| id == o))
        .max();
    let mut out = match deepest {
        Some(pos) => legacy_newest_first[..=pos].to_vec(),
        None => Vec::new(),
    };
    for o in owed {
        if !out.contains(o) {
            out.push(*o);
        }
    }
    out
}

/// The generations a background re-key merge asks: only the owed ones, newest
/// first (registry order, then any unregistered owed key). A retry, or the
/// one-time merge from `previous_contract_key`, has no blocking seed to protect, and walking
/// every skipped generation again on each load would grow with the registry.
pub(crate) fn owed_candidates(
    legacy_newest_first: &[ContractInstanceId],
    owed: &[ContractInstanceId],
) -> Vec<ContractInstanceId> {
    let mut out: Vec<ContractInstanceId> = legacy_newest_first
        .iter()
        .filter(|id| owed.contains(id))
        .copied()
        .collect();
    for o in owed {
        if !out.contains(o) {
            out.push(*o);
        }
    }
    out
}

/// What a finished re-key merge does (freenet/river#769). Pure, so each arm is
/// testable without a runtime.
#[derive(Debug)]
pub(crate) struct RekeyFinish {
    /// Older-generation state to CRDT-merge into the room, if any was found.
    pub recovered: Option<ChatRoomStateV1>,
    /// Owed generations that answered with the room's state, which the fold
    /// kept: these come off the marker once the merge has landed.
    pub satisfied: Vec<ContractInstanceId>,
    /// Every owed generation is satisfied.
    pub complete: bool,
}

/// Only an owed generation that answered with real state is satisfied. Each
/// held this room (the device synced under it), so a `NotFound` or silence
/// from it means "could not read it", never "it is empty"; a `NotFound` on
/// Freenet is routinely a routing dead-end. Nor does one owed key answering
/// vouch for another: what reached a newer key is not on an older one. A walk
/// that leaves any owed key unsatisfied still merges what it found and seeds,
/// and the next load asks again. If folding two generations failed, nothing
/// is satisfied.
///
/// The answer for an old key may come from this node's own stored copy rather
/// than from other peers: the device subscribed to that key before the re-key,
/// so its node usually still hosts it. That copy is counted as the old key's
/// state. freenet-core keeps a hosted copy in the contract's update mesh while
/// the node is connected, so it is current; on a node isolated since the
/// re-key it can be stale, and the key comes off the marker having merged only
/// what the node held. What it missed is usually carried forward by its
/// authors when they upgrade; writers on an old riverctl, bots and members who
/// left do not.
pub(crate) fn plan_rekey_finish(
    outcome: Outcome<ChatRoomStateV1>,
    owed: &[ContractInstanceId],
    hits: &[ContractInstanceId],
    fold_failed: bool,
) -> RekeyFinish {
    let recovered = match outcome {
        Outcome::Recovered { merged, .. } => Some(merged),
        // No generation had real state, so no owed one did either.
        // `Indeterminate` (freenet-migrate 0.6) is the same with some
        // generation silent; `Outcome` is `#[non_exhaustive]`, and an arm this
        // build has not reasoned about recovers and satisfies nothing.
        Outcome::SeedLocal { .. } | Outcome::NoLegacy { .. } | Outcome::Indeterminate { .. } => {
            None
        }
        _ => None,
    };
    let satisfied: Vec<ContractInstanceId> = if recovered.is_none() || fold_failed {
        Vec::new()
    } else {
        owed.iter().filter(|o| hits.contains(o)).copied().collect()
    };
    RekeyFinish {
        complete: !owed.is_empty() && satisfied.len() == owed.len(),
        recovered,
        satisfied,
    }
}

/// Turn a background re-key merge into one that seeds the room when it ends.
/// Returns whether it changed anything.
fn upgrade_to_seed(entry: &mut ProbeEntry) -> bool {
    match &mut entry.purpose {
        ProbePurpose::RekeyMerge { seed } if !*seed => {
            *seed = true;
            true
        }
        _ => false,
    }
}

/// [`upgrade_to_seed`], and since the room is now held out of the subscribe
/// sweep, refresh its timeout: the hop already in flight was fired without
/// touching it.
fn make_seed(entry: &mut ProbeEntry, owner_vk: VerifyingKey) {
    if upgrade_to_seed(entry) {
        crate::util::defer(move || {
            SYNC_INFO.with_mut(|sync_info| sync_info.touch_subscribing_since(&owner_vk));
        });
        arm_seed_deadline(owner_vk, entry.walk);
    }
}

/// The synchronous half of a seed deadline: if walk `walk` for `owner_vk` is
/// still running and still holds its room's seed, let go of it, so the walk
/// finishes in the background. Returns whether the caller must seed now.
fn take_seed_deadline(owner_vk: VerifyingKey, walk: u64) -> bool {
    let mut drivers = drivers();
    match drivers.get_mut(&owner_vk) {
        Some(entry)
            if entry.walk == walk && entry.purpose == (ProbePurpose::RekeyMerge { seed: true }) =>
        {
            entry.purpose = ProbePurpose::RekeyMerge { seed: false };
            true
        }
        _ => false,
    }
}

/// Bound how long a re-key merge holds its room's seed PUT to
/// [`REKEY_SEED_WAIT`]. At the deadline a walk still running seeds the room
/// with what it holds now and carries on in the background; its merge later
/// reaches the new key through `mark_needs_sync`.
fn arm_seed_deadline(owner_vk: VerifyingKey, walk: u64) {
    safe_spawn_local(async move {
        sleep(REKEY_SEED_WAIT).await;
        if take_seed_deadline(owner_vk, walk) {
            info!(
                "Re-key merge for room {:?} still running after {}s — seeding the new key \
                 now and finishing the merge in the background",
                MemberId::from(owner_vk),
                REKEY_SEED_WAIT.as_secs()
            );
            crate::util::defer(move || {
                SYNC_INFO.with_mut(|sync_info| sync_info.require_seed_put(&owner_vk));
                request_seed_put(owner_vk);
            });
        }
    });
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
/// `owed` is the room's persisted `rekey_merge.owed`: the older contract ids
/// it was synced under and still owes a merge from. With `seed`, the caller
/// must hold the room out of the subscribe sweep (status `Subscribing`): the
/// seed PUT is the merge's last step, so it carries the old key's messages
/// rather than only this device's snapshot. The walk seeds when it ends or at
/// [`REKEY_SEED_WAIT`], whichever comes first. Without `seed`, only the owed
/// keys are asked, and nothing waits on the walk.
pub async fn start_rekey_merge(
    owner_vk: VerifyingKey,
    owed: Vec<ContractInstanceId>,
    local_snapshot: ChatRoomStateV1,
    seed: bool,
) -> RekeyStart {
    {
        let mut drivers = drivers();
        if let Some(entry) = drivers.get_mut(&owner_vk) {
            // A `Recover` probe seeds on its own; a background merge is told to.
            if seed {
                make_seed(entry, owner_vk);
            }
            return RekeyStart::AlreadyRunning;
        }
    }
    if owed.is_empty() || !rekey_attempted().insert(owner_vk) {
        return RekeyStart::AlreadyAttempted;
    }

    let legacy: Vec<ContractInstanceId> = owner_vk_to_legacy_contract_keys(&owner_vk)
        .iter()
        .map(|k| *k.id())
        .collect();
    // A seeding walk is the first load after a re-key: it also asks the
    // generations a device that skipped releases never synced under, and is
    // bounded by the seed deadline. A background walk asks only what is owed.
    let candidates = if seed {
        rekey_candidates(&legacy, &owed)
    } else {
        owed_candidates(&legacy, &owed)
    };
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
    // Both candidate lists keep the registry's newest-first order.
    let entry = ProbeEntry::new(
        owner_vk,
        local_snapshot,
        candidates,
        ProbePurpose::RekeyMerge { seed },
        owed,
    );
    let walk = entry.walk;
    drivers().insert(owner_vk, entry);
    if seed {
        arm_seed_deadline(owner_vk, walk);
    }
    pump_probe(owner_vk).await;
    RekeyStart::Started
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
    // Don't start a second probe for an owner that already has one running.
    // Return `true` so the caller still treats recovery as in-flight. A probe's
    // driver entry lives from here until its terminal Outcome (removed in
    // `pump_probe`'s Done branch), so a completed probe leaves no entry and a
    // later probe for the same owner can start fresh.
    //
    // A running re-key merge (freenet/river#769) covers this room's newer
    // generations; the current key is empty, so it must now end in a seed.
    if let Some(entry) = drivers().get_mut(&owner_vk) {
        make_seed(entry, owner_vk);
        info!(
            "Backward probe already in progress for {:?}, not restarting",
            MemberId::from(owner_vk)
        );
        return true;
    }

    let legacy_keys = owner_vk_to_legacy_contract_keys(&owner_vk);
    if legacy_keys.is_empty() {
        info!(
            "No legacy room-contract generations — skipping backward probe for {:?}",
            MemberId::from(owner_vk)
        );
        return false;
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
    // River adopts exactly one generation (the newest real one) and never
    // reads older ones — safe for delete-by-absence state (pruned messages
    // stay pruned). The 64-hop cap is the driver default (asserted above).
    drivers().insert(
        owner_vk,
        ProbeEntry::new(
            owner_vk,
            local_snapshot,
            legacy_keys.iter().map(|k| *k.id()).collect(),
            ProbePurpose::Recover,
            Vec::new(),
        ),
    );
    pump_probe(owner_vk).await;
    true
}

/// Deliver a typed `NotFound` for a legacy-key probe GET. The walk moves on
/// now rather than after the watchdog. It is not evidence the generation is
/// empty (see [`plan_rekey_finish`]); it only stops the walk waiting on it.
pub(crate) async fn deliver_probe_absent(id: ContractInstanceId) -> bool {
    match take_probe_absent(id) {
        ProbeReply::Advance(owner_vk) => {
            pump_probe(owner_vk).await;
            true
        }
        ProbeReply::Consumed => true,
        ProbeReply::NotOurs => false,
    }
}

/// What a reply for a contract id meant to the backward probe.
#[derive(Debug, PartialEq, Eq)]
enum ProbeReply {
    /// A probe was waiting on it and has taken it; pump that owner's walk.
    Advance(VerifyingKey),
    /// A late answer to a probe GET the watchdog had given up on: nothing to
    /// do, but it is the probe's, not some room's.
    Consumed,
    /// Not a probe reply.
    NotOurs,
}

/// The synchronous half of [`deliver_probe_absent`]. A `NotFound` for an id
/// with an abandoned fire outstanding ([`ORPHANED_FIRES`]) may be that fire's,
/// so it consumes the orphan and is ignored, routed or not: a later fire that
/// re-registered the id falls back to its own watchdog. Otherwise it claims
/// `id`'s route and advances the walk past `id`.
fn take_probe_absent(id: ContractInstanceId) -> ProbeReply {
    if take_orphaned_fire(&id) {
        info!("NotFound for {id} may answer an abandoned probe GET — not trusting it");
        return ProbeReply::Consumed;
    }
    let Some((owner_vk, _token)) = routes().remove(&id) else {
        return ProbeReply::NotOurs;
    };
    info!("Legacy contract {id} answered NotFound — advancing to older generation");
    if let Some(entry) = drivers().get_mut(&owner_vk) {
        // A typed answer from the node: `on_absent` (freenet-migrate 0.6). It
        // advances the walk and is recorded as no state, never as a hit.
        entry.driver.on_absent(id);
    }
    ProbeReply::Advance(owner_vk)
}

/// Deliver a legacy-key GET response into its owner's probe driver, then pump.
/// The response's contract id (`id`) was registered by [`fire_probe_get`]; the
/// FIRST of {this response, the watchdog} to remove the route owns the hop
/// (single-shot). Unknown ids are the race-loser and no-op.
pub(crate) async fn deliver_probe_response(id: ContractInstanceId, bytes: &[u8]) -> bool {
    match take_probe_response(id, bytes) {
        ProbeReply::Advance(owner_vk) => {
            pump_probe(owner_vk).await;
            true
        }
        ProbeReply::Consumed => true,
        ProbeReply::NotOurs => false,
    }
}

/// The synchronous half of [`deliver_probe_response`]: claim `id`'s route and
/// feed the bytes to its driver. Returns the owner to pump.
fn take_probe_response(id: ContractInstanceId, bytes: &[u8]) -> ProbeReply {
    // The per-fire token is deliberately IGNORED here (unlike the watchdog): a
    // GET response for `id` is a valid response for whatever fire currently owns
    // `id`'s route, and the driver is single-shot on its own outstanding
    // candidate — if the owning driver has already advanced past `id`, its
    // `on_response(id, ..)` is a no-op. So routing by the current owner is
    // always correct; only the stale-timeout case needs the token.
    // One reply consumed for `id`, whichever fire sent it.
    let orphan = take_orphaned_fire(&id);
    let Some((owner_vk, _token)) = routes().remove(&id) else {
        // The route was already consumed — e.g. the timeout watchdog fired
        // first. Whichever ran first owns the outcome; the loser lands here.
        return if orphan {
            info!("Late probe GET response for {id} after its watchdog — ignoring");
            ProbeReply::Consumed
        } else {
            ProbeReply::NotOurs
        };
    };
    {
        let mut drivers = drivers();
        if let Some(ProbeEntry { driver, hits, .. }) = drivers.get_mut(&owner_vk) {
            // Observability only — the driver makes the actual decision. This
            // re-classifies the bytes purely to keep the pre-driver era's
            // per-candidate logs (once per probe hop, cost negligible).
            let ops = RiverProbeOps::new(owner_vk);
            match ops.decode(bytes) {
                None => warn!(
                    "Legacy contract {id} returned undeserializable state — skipping generation"
                ),
                Some(state) if !ops.is_real(&state) => {
                    info!("Legacy contract {id} is empty — advancing to older generation")
                }
                Some(_) => {
                    if !hits.contains(&id) {
                        hits.push(id);
                    }
                }
            }
            driver.on_response(id, bytes);
        }
    }
    ProbeReply::Advance(owner_vk)
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
            if let ProbePurpose::RekeyMerge { seed } = entry.purpose {
                if let Some(outcome) = outcome {
                    let plan = plan_rekey_finish(
                        outcome,
                        &entry.owed,
                        &entry.hits,
                        entry.fold_failed.load(Ordering::Relaxed),
                    );
                    finish_rekey_merge(owner_vk, seed, plan);
                }
                return;
            }
            match probe_finish(outcome) {
                ProbeFinish::Adopt(merged, source) => {
                    info!(
                        "Backward probe for room {:?} recovered real state from legacy contract \
                         {} — adopting and PUTting forward onto the current key",
                        MemberId::from(owner_vk),
                        source
                    );
                    adopt_recovered_probe_state(owner_vk, merged).await;
                }
                // Exhaustion (SeedLocal, or Indeterminate: see `probe_finish`)
                // and the no-legacy edge (NoLegacy, which `start_backward_probe`
                // guards against but the driver can still produce) all PUT the
                // device's local snapshot forward as the genuine last resort —
                // the only path on which local state is seeded onto the network
                // (the core design principle of #292).
                ProbeFinish::SeedLocal(local)
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
                ProbeFinish::SeedLocal(local) => {
                    info!(
                        "Backward probe for room {:?} exhausted — seeding current contract key \
                         with the local snapshot (last resort)",
                        MemberId::from(owner_vk)
                    );
                    seed_current_key_with_local(owner_vk, local).await;
                }
                ProbeFinish::Nothing => {
                    // Outcome already taken (a duplicate pump), or a variant
                    // this build does not know — nothing to do.
                }
            }
        }
    }
}

/// Apply a finished re-key merge (freenet/river#769): fold what the older keys
/// held into the room, clear the marker if the walk heard from every
/// generation, and, if asked, re-seed the current key.
fn finish_rekey_merge(owner_vk: VerifyingKey, seed: bool, plan: RekeyFinish) {
    if plan.complete {
        info!(
            "Re-key merge for room {:?} finished: every older key it owed returned the room",
            MemberId::from(owner_vk)
        );
    } else {
        warn!(
            "Re-key merge for room {:?}: {} of the older key(s) it last synced under did not \
             return the room — merging what was found and asking again on the next load",
            MemberId::from(owner_vk),
            if plan.satisfied.is_empty() {
                "all"
            } else {
                "some"
            }
        );
    }
    let RekeyFinish {
        recovered,
        satisfied,
        ..
    } = plan;
    crate::util::defer(move || {
        ROOMS.with_mut(|rooms| {
            if let Some(room_data) = rooms.map.get_mut(&owner_vk) {
                apply_rekey_finish(
                    room_data,
                    recovered,
                    &satisfied,
                    owner_vk,
                    crate::util::get_current_system_time(),
                );
            }
        });
    });
    // Save the merged room and the updated marker together (one room slot),
    // and let a subscribed room push the merged-in messages as a delta. FIFO
    // `defer` puts this after the merge above. Always: even a walk that found
    // nothing changes the marker's attempt count.
    crate::components::app::mark_needs_sync(owner_vk);
    if seed {
        // After the merge (FIFO): the seed PUT reads the room from ROOMS, so it
        // carries the old key's messages and anything typed meanwhile.
        crate::util::defer(move || {
            SYNC_INFO.with_mut(|sync_info| sync_info.require_seed_put(&owner_vk));
            request_seed_put(owner_vk);
        });
    }
}

/// Fold a finished re-key merge into the room: merge the recovered state (a
/// CRDT merge, never a replace), then take the owed keys that answered off the
/// marker, but only if that merge landed. A merge that fails leaves the room
/// and the owed keys as they were, so the next load tries again. A walk that
/// leaves keys owed counts toward [`MAX_UNFINISHED_REKEY_LOADS`]; the rest are
/// given up once that many loads AND [`REKEY_GIVE_UP_AFTER`] have passed.
/// Returns whether the merge landed.
pub(crate) fn apply_rekey_finish(
    room_data: &mut crate::room_data::RoomData,
    recovered: Option<ChatRoomStateV1>,
    satisfied: &[ContractInstanceId],
    owner_vk: VerifyingKey,
    now: std::time::SystemTime,
) -> bool {
    // The old key's own upgrade pointer means nothing under the current key
    // (freenet/river#427), so it stays out of the room.
    let merged = match recovered {
        Some(state) => merge_recovered_into_room(
            room_data,
            crate::util::strip_upgrade_pointer(&state),
            owner_vk,
        ),
        None => true,
    };
    let marker = &mut room_data.rekey_merge;
    if merged {
        marker.owed.retain(|k| !satisfied.contains(k.id()));
    }
    let now_ms = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if marker.owed.is_empty() {
        marker.unfinished_loads = 0;
        marker.first_owed_at = None;
    } else {
        marker.unfinished_loads += 1;
        let since = *marker.first_owed_at.get_or_insert(now_ms);
        let owed_for = Duration::from_millis(now_ms.saturating_sub(since));
        if marker.unfinished_loads >= MAX_UNFINISHED_REKEY_LOADS && owed_for >= REKEY_GIVE_UP_AFTER
        {
            warn!(
                "Re-key merge for room {:?}: {} older key(s) did not return the room in {} \
                 loads over {} hours — no longer asking",
                MemberId::from(owner_vk),
                marker.owed.len(),
                marker.unfinished_loads,
                owed_for.as_secs() / 3600
            );
            marker.owed.clear();
            marker.unfinished_loads = 0;
            marker.first_owed_at = None;
        }
    }
    merged
}

/// The synchronous half of a probe GET's watchdog: if this fire still owns
/// `id`'s route, give up on it and advance the walk. Returns whether it did.
fn take_probe_timeout(id: ContractInstanceId, owner_vk: VerifyingKey, token: u64) -> bool {
    if !claim_route_if_current(id, owner_vk, token) {
        return false;
    }
    warn!(
        "Backward-probe GET for {id} timed out after {}s — advancing past the legacy \
         generation (its state is unknown)",
        PROBE_GET_TIMEOUT.as_secs()
    );
    // The GET may still be in flight; see `ORPHANED_FIRES`. One that was
    // never sent cannot answer late.
    if !unsent_fires().remove(&token) {
        *orphaned_fires().entry(id).or_insert(0) += 1;
    }
    if let Some(entry) = drivers().get_mut(&owner_vk) {
        // `on_unknown`, never `on_absent`: a timeout is silence, not an
        // answered NotFound (freenet-migrate#19).
        entry.driver.on_unknown(id);
    }
    true
}

/// Register `id` as the outstanding probe GET for `owner_vk`, send the GET, and
/// arm a watchdog so an absent legacy generation cannot stall the probe.
fn fire_probe_get(owner_vk: VerifyingKey, id: ContractInstanceId) {
    // A fresh per-fire token: it stamps this route and is captured by the
    // watchdog below, so a stale watchdog from a completed probe cannot advance
    // a LATER probe that reused the same deterministic legacy id.
    let token = next_fire_token();

    // Register the route BEFORE sending the GET so the response handler can
    // resolve the legacy key when the reply arrives (and so `take_probe_response`
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
    if drivers()
        .get(&owner_vk)
        .is_some_and(ProbeEntry::holds_subscription)
    {
        crate::util::defer(move || {
            SYNC_INFO.with_mut(|sync_info| {
                sync_info.touch_subscribing_since(&owner_vk);
            });
        });
    }

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
        let sent = if let Some(web_api) = crate::components::app::WEB_API.write().as_mut() {
            match web_api.send(ClientRequest::ContractOp(get_request)).await {
                Ok(()) => true,
                Err(e) => {
                    warn!("Failed to send backward-probe GET for {id}: {e}");
                    false
                }
            }
        } else {
            warn!("WebAPI unavailable — backward-probe GET for {id} not sent");
            false
        };
        if !sent {
            // Leave the route in place: the watchdog below advances the probe,
            // so a transient send failure still walks to the next generation
            // rather than silently abandoning recovery. Nothing is in flight,
            // so the watchdog must not expect a late reply.
            unsent_fires().insert(token);
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
        if take_probe_timeout(id, owner_vk, token) {
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

    /// A contract key whose instance id is `id(n)`, for a room's marker.
    fn contract_key_with_id(n: u8) -> freenet_stdlib::prelude::ContractKey {
        freenet_stdlib::prelude::ContractKey::from_id_and_code(
            id(n),
            freenet_stdlib::prelude::CodeHash::new([n; 32]),
        )
    }

    /// `RiverProbeOps` classification is exactly what the deleted
    /// `handle_probe_get_response` applied inline: a default/placeholder state
    /// is NOT real (its signature does not verify → a miss), an owner-signed
    /// state IS real (a hit), and undecodable bytes decode to `None` (a miss so
    /// the probe advances rather than panicking).
    #[test]
    fn river_probe_ops_classifies_real_decodable_and_garbage() {
        let (sk, vk) = owner(1);
        let ops = RiverProbeOps::new(vk);

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
        let ops = RiverProbeOps::new(vk);

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
        let ops = RiverProbeOps::new(vk);
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
        let mut driver = new_probe_driver(
            RiverProbeOps::new(vk),
            ChatRoomStateV1::default(),
            vec![id(9), id(5)],
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
    /// no-silent-data-loss guarantee on exhaustion. Under freenet-migrate 0.6
    /// the timeout makes the outcome `Indeterminate`, which River maps to the
    /// same seed (`probe_finish`).
    #[test]
    fn driver_exhausts_to_seed_local_with_river_ops() {
        let (sk, vk) = owner(5);
        let local = signed_state(&sk);
        let mut driver = new_probe_driver(
            RiverProbeOps::new(vk),
            local,
            vec![id(2), id(1)],
            SelectionPolicy::NewestFirstWins,
        );
        let Step::Get(a) = driver.next_action() else {
            panic!()
        };
        driver.on_unknown(a); // silent generation (watchdog)
        let Step::Get(b) = driver.next_action() else {
            panic!("an unknown newest generation must not halt the walk (continue_past_unknown)")
        };
        driver.on_response(b, &encode(&ChatRoomStateV1::default())); // empty → miss
        assert_eq!(driver.next_action(), Step::Done);
        let outcome = driver.take_outcome();
        assert!(
            matches!(outcome, Some(Outcome::Indeterminate { .. })),
            "a timed-out candidate leaves the outcome indeterminate, never SeedLocal"
        );
        let ProbeFinish::SeedLocal(seeded) = probe_finish(outcome) else {
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
        let mut driver = new_probe_driver(
            RiverProbeOps::new(vk),
            ChatRoomStateV1::default(),
            vec![id(2), id(1)],
            SelectionPolicy::NewestFirstWins,
        );
        let Step::Get(a) = driver.next_action() else {
            panic!()
        };
        driver.on_unknown(a);
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
            matches!(driver.take_outcome(), Some(Outcome::Indeterminate { .. })),
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
        let mut probe2 = ProbeEntry::new(
            vk,
            ChatRoomStateV1::default(),
            vec![newest, older],
            ProbePurpose::Recover,
            Vec::new(),
        );
        assert_eq!(
            probe2.driver.next_action(),
            Step::Get(newest),
            "probe 2 starts on its newest candidate"
        );
        drivers().insert(vk, probe2);

        // --- W1 fires LATE. Run the watchdog's EXACT gate: it advances
        // (on_unknown + pump) ONLY if `claim_route_if_current` returns true. ---
        let w1_claimed = claim_route_if_current(newest, vk, t1);
        if w1_claimed {
            // The buggy path this guards: this would advance probe 2 past its
            // newest candidate onto `older` — a silent rollback.
            drivers().get_mut(&vk).unwrap().driver.on_unknown(newest);
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
            rekey_merge: Default::default(),
            invitation_secrets: std::collections::BTreeMap::new(),
        }
    }

    /// How one probe GET of the walk ends, in [`walk_rekey`].
    enum Reply {
        State(ChatRoomStateV1),
        /// Raw bytes, for an answer that is not a decodable room state.
        Bytes(Vec<u8>),
        NotFound,
        Silent,
    }

    /// Run a re-key merge walk to completion through the REAL adapter: the
    /// shared `PROBE_DRIVERS` entry and route table, and the synchronous halves
    /// of the response, `NotFound` and watchdog handlers (everything but the
    /// network send and the pump's async tail). `owed` are the keys the room
    /// owes a merge from. Returns what the merge would apply.
    fn walk_rekey(
        owner_sk: &SigningKey,
        local: ChatRoomStateV1,
        legacy: &[ContractInstanceId],
        owed: Vec<ContractInstanceId>,
        replies: Vec<Reply>,
    ) -> RekeyFinish {
        let vk = owner_sk.verifying_key();
        let candidates = rekey_candidates(legacy, &owed);
        let purpose = ProbePurpose::RekeyMerge { seed: true };
        drivers().insert(
            vk,
            ProbeEntry::new(vk, local, candidates.clone(), purpose, owed),
        );
        let mut replies = replies.into_iter();
        loop {
            let step = drivers().get_mut(&vk).unwrap().driver.next_action();
            let Step::Get(id) = step else { break };
            let token = next_fire_token();
            routes().insert(id, (vk, token));
            let taken = match replies.next().expect("a reply per GET") {
                Reply::State(state) => take_probe_response(id, &encode(&state)),
                Reply::Bytes(bytes) => take_probe_response(id, &bytes),
                Reply::NotFound => take_probe_absent(id),
                Reply::Silent => {
                    assert!(take_probe_timeout(id, vk, token));
                    ProbeReply::Advance(vk)
                }
            };
            assert_eq!(
                taken,
                ProbeReply::Advance(vk),
                "each reply claims its route"
            );
        }
        assert!(
            replies.next().is_none(),
            "every scripted reply was asked for"
        );
        let mut entry = drivers().remove(&vk).unwrap();
        assert_eq!(entry.purpose, purpose);
        // Late replies to the GETs the watchdog abandoned would arrive after
        // the walk; consume this walk's (only) so no other test sees them.
        for id in &candidates {
            while take_orphaned_fire(id) {}
        }
        plan_rekey_finish(
            entry.driver.take_outcome().unwrap(),
            &entry.owed,
            &entry.hits,
            entry.fold_failed.load(Ordering::Relaxed),
        )
    }

    fn strs(v: &[&str]) -> Vec<String> {
        let mut out: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        out.sort();
        out
    }

    /// Only the generations from the newest legacy one down to the oldest
    /// owed one are asked, newest first; an unregistered owed key is asked too.
    #[test]
    fn rekey_candidates_stop_at_the_oldest_owed_generation() {
        let legacy = vec![id(30), id(29), id(28), id(27)];
        assert_eq!(rekey_candidates(&legacy, &[id(30)]), vec![id(30)]);
        assert_eq!(
            rekey_candidates(&legacy, &[id(28)]),
            vec![id(30), id(29), id(28)],
            "skipped releases are asked newest first, down to the device's own"
        );
        assert_eq!(
            rekey_candidates(&legacy, &[id(29), id(28)]),
            vec![id(30), id(29), id(28)]
        );
        assert_eq!(rekey_candidates(&legacy, &[id(99)]), vec![id(99)]);
        assert_eq!(
            rekey_candidates(&legacy, &[id(29), id(99)]),
            vec![id(30), id(29), id(99)]
        );
        assert_eq!(rekey_candidates(&[], &[id(5)]), vec![id(5)]);
    }

    /// The rehearsal's failure: 16 messages on the old key, 10 in this
    /// device's snapshot. Every one of the 16 must reach the state the seed PUT
    /// carries, and the owed key comes off the marker.
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
            &[id(81)],
            vec![id(81)],
            vec![Reply::State(room_with_messages(&sk, &old_refs))],
        );
        assert!(finish.complete, "the old key returned the room: done");
        assert_eq!(finish.satisfied, vec![id(81)]);

        let mut room = room_data(&sk, local_state);
        room.rekey_merge.owed = vec![contract_key_with_id(81)];
        assert!(apply_rekey_finish(
            &mut room,
            finish.recovered,
            &finish.satisfied,
            sk.verifying_key(),
            std::time::SystemTime::now()
        ));
        let mut expected = old_key.clone();
        expected.sort();
        assert_eq!(texts(&room.room_state), expected, "all 16 messages survive");
        assert!(
            room.rekey_merge.owed.is_empty(),
            "the merge is no longer owed"
        );
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
        assert_eq!(
            texts(&room.room_state),
            strs(&["m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "m20"])
        );
    }

    /// A recovered state that does not merge leaves the room exactly as it
    /// was AND keeps the marker, so the next load tries again.
    #[test]
    fn a_failed_rekey_merge_changes_nothing_and_stays_owed() {
        let sk = SigningKey::from_bytes(&[88u8; 32]);
        let before = room_with_messages(&sk, &["m1", "m2"]);
        let mut room = room_data(&sk, before.clone());
        room.rekey_merge.owed = vec![contract_key_with_id(88)];
        // The recovered state carries a member record attributed to the
        // owner's invitation that the owner did not sign, so the merge fails.
        // (Earlier fixtures used a public message in a room made private,
        // then a configuration the owner did not sign; since
        // freenet/river#703 the room drops or never offers those.)
        let mut bad = room_with_messages(&sk, &["m1", "m9"]);
        bad.members
            .members
            .push(river_core::room_state::member::AuthorizedMember {
                member: river_core::room_state::member::Member {
                    owner_member_id: sk.verifying_key().into(),
                    invited_by: sk.verifying_key().into(),
                    member_vk: SigningKey::from_bytes(&[0x5b; 32]).verifying_key(),
                },
                signature: ed25519_dalek::Signature::from_bytes(&[0; 64]),
            });
        let mut probe = before.clone();
        assert!(
            probe
                .merge(
                    &before,
                    &ChatRoomParametersV1 {
                        owner: sk.verifying_key()
                    },
                    &bad
                )
                .is_err(),
            "fixture: this merge must fail"
        );
        assert!(!apply_rekey_finish(
            &mut room,
            Some(bad),
            &[id(88)],
            sk.verifying_key(),
            std::time::SystemTime::now()
        ));
        assert_eq!(
            room.room_state, before,
            "no half-merged state is left behind"
        );
        assert_eq!(room.rekey_merge.owed, vec![contract_key_with_id(88)]);
    }

    /// The owed old key does not return the room, whether it stays silent or
    /// answers `NotFound`: nothing is merged, the local state seeds on its own,
    /// and the merge stays owed for the next load. The device synced the room
    /// under that key, so neither answer can mean it is empty; a `NotFound` on
    /// Freenet is routinely a dead-end.
    #[test]
    fn rekey_merge_without_the_old_keys_state_keeps_local_and_retries() {
        let sk = SigningKey::from_bytes(&[83u8; 32]);
        let local = room_with_messages(&sk, &["m1", "m2"]);
        for (n, reply) in [(83u8, Reply::Silent), (84, Reply::NotFound)] {
            let finish = walk_rekey(&sk, local.clone(), &[id(n)], vec![id(n)], vec![reply]);
            assert!(
                finish.recovered.is_none(),
                "no state arrived, none is merged"
            );
            assert!(finish.satisfied.is_empty());
            assert!(!finish.complete, "the merge must stay owed");
        }
    }

    /// A device that skipped releases, where the newer generations hold state
    /// too (members who upgraded earlier seeded them). Every generation down
    /// to the one this device last synced under is folded in, so messages only
    /// that one holds are not shadowed by a newer generation that has state.
    #[test]
    fn rekey_merge_folds_every_generation_down_to_the_owed_one() {
        let sk = SigningKey::from_bytes(&[85u8; 32]);
        let local = room_with_messages(&sk, &["m1"]);
        let legacy = [id(90), id(89), id(88), id(87)];

        let finish = walk_rekey(
            &sk,
            local.clone(),
            &legacy,
            vec![id(88)],
            vec![
                Reply::State(room_with_messages(&sk, &["m1", "m2"])),
                Reply::NotFound,
                Reply::State(room_with_messages(&sk, &["m1", "m3", "m4"])),
            ],
        );
        assert!(finish.complete, "the owed generation returned the room");
        assert_eq!(
            texts(finish.recovered.as_ref().expect("state was found")),
            strs(&["m1", "m2", "m3", "m4"]),
            "the newest generation's state did not shadow the owed one's"
        );

        // The owed generation never answered: what the newer one had is still
        // merged (a merge cannot lose anything), but the merge stays owed.
        let finish = walk_rekey(
            &sk,
            local,
            &legacy,
            vec![id(88)],
            vec![
                Reply::State(room_with_messages(&sk, &["m1", "m2"])),
                Reply::Silent,
                Reply::Silent,
            ],
        );
        assert_eq!(
            texts(
                finish
                    .recovered
                    .as_ref()
                    .expect("the newest generation's state")
            ),
            strs(&["m1", "m2"])
        );
        assert!(!finish.complete);
        assert!(finish.satisfied.is_empty());
    }

    /// Two re-keys, the first merge still owed: the device synced under BOTH
    /// older keys, and each must answer on its own. The older one answering
    /// does not vouch for the newer one (what reached the newer key is not on
    /// the older), so only the older comes off the marker.
    #[test]
    fn each_owed_generation_must_answer_for_itself() {
        let sk = SigningKey::from_bytes(&[91u8; 32]);
        let local = room_with_messages(&sk, &["m1"]);
        let legacy = [id(93), id(92), id(91)];
        let finish = walk_rekey(
            &sk,
            local,
            &legacy,
            vec![id(91), id(92)],
            vec![
                Reply::State(room_with_messages(&sk, &["m1", "m5"])),
                Reply::NotFound,
                Reply::State(room_with_messages(&sk, &["m1", "m2"])),
            ],
        );
        assert_eq!(finish.satisfied, vec![id(91)], "only the key that answered");
        assert!(!finish.complete, "the newer owed key still owes");
        assert_eq!(
            texts(finish.recovered.as_ref().unwrap()),
            strs(&["m1", "m2", "m5"])
        );
    }

    /// A `NotFound` carries no fire token. One arriving for a legacy id whose
    /// earlier GET the watchdog abandoned may be that abandoned fire's answer,
    /// so it must not advance a later probe that re-registered the id (that
    /// probe falls back to its own watchdog), and a late one with no route at
    /// all is consumed, not mistaken for a room's. A fire that was never sent
    /// leaves no orphan.
    #[test]
    fn a_not_found_that_may_answer_an_abandoned_fire_is_not_trusted() {
        let sk = SigningKey::from_bytes(&[87u8; 32]);
        let vk = sk.verifying_key();
        let gen = id(110);
        let fire = |token: u64| {
            routes().insert(gen, (vk, token));
            drivers().insert(
                vk,
                ProbeEntry::new(
                    vk,
                    ChatRoomStateV1::default(),
                    vec![gen],
                    ProbePurpose::Recover,
                    Vec::new(),
                ),
            );
            assert_eq!(
                drivers().get_mut(&vk).unwrap().driver.next_action(),
                Step::Get(gen)
            );
        };

        // Fire 1 times out: its GET may still be in flight.
        let t1 = next_fire_token();
        fire(t1);
        assert!(take_probe_timeout(gen, vk, t1));
        drivers().remove(&vk);
        // Its late NotFound arrives with no probe waiting: consumed.
        assert_eq!(take_probe_absent(gen), ProbeReply::Consumed);
        assert_eq!(take_probe_absent(gen), ProbeReply::NotOurs);

        // Fire 2 times out too; fire 3 re-registers the id.
        let t2 = next_fire_token();
        fire(t2);
        assert!(take_probe_timeout(gen, vk, t2));
        let t3 = next_fire_token();
        fire(t3);
        // The first NotFound may be fire 2's: ignored, the route survives.
        assert_eq!(take_probe_absent(gen), ProbeReply::Consumed);
        assert_eq!(routes().get(&gen), Some(&(vk, t3)));
        assert_eq!(
            drivers().get_mut(&vk).unwrap().driver.next_action(),
            Step::Get(gen)
        );
        // The next one is fire 3's: it advances the walk.
        assert_eq!(take_probe_absent(gen), ProbeReply::Advance(vk));
        assert!(routes().get(&gen).is_none());
        assert_eq!(
            drivers().get_mut(&vk).unwrap().driver.next_action(),
            Step::Done
        );
        drivers().remove(&vk);

        // A fire that was never sent cannot answer late: no orphan.
        let t4 = next_fire_token();
        fire(t4);
        unsent_fires().insert(t4);
        assert!(take_probe_timeout(gen, vk, t4));
        drivers().remove(&vk);
        assert_eq!(take_probe_absent(gen), ProbeReply::NotOurs);
    }

    /// A walk already running in the background for an owner is told to seed
    /// when that room's current key turns out to be empty or missing, rather
    /// than a second walk starting or the room going unseeded. Both entry
    /// points route through `make_seed` (pinned in the source check below);
    /// `upgrade_to_seed` is its runtime-free half.
    #[test]
    fn an_empty_current_key_makes_a_background_merge_seed() {
        let vk = SigningKey::from_bytes(&[86u8; 32]).verifying_key();
        let mut entry = ProbeEntry::new(
            vk,
            ChatRoomStateV1::default(),
            vec![id(86)],
            ProbePurpose::RekeyMerge { seed: false },
            vec![id(86)],
        );
        assert!(
            !entry.holds_subscription(),
            "a background merge must not stretch the room's own subscribe timeout"
        );
        assert!(upgrade_to_seed(&mut entry));
        assert_eq!(entry.purpose, ProbePurpose::RekeyMerge { seed: true });
        assert!(entry.holds_subscription(), "a seeding merge holds the room");
        assert!(
            !upgrade_to_seed(&mut entry),
            "already seeding: no second touch"
        );
        let mut recover = ProbeEntry::new(
            vk,
            ChatRoomStateV1::default(),
            vec![id(86)],
            ProbePurpose::Recover,
            Vec::new(),
        );
        assert!(
            !upgrade_to_seed(&mut recover),
            "a #292 probe seeds on its own"
        );

        let src: String = crate::util::strip_comments(
            include_str!("backward_probe.rs")
                .split("mod tests {")
                .next()
                .unwrap(),
        )
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
        let section = |head: &str, next: &str| {
            let start = src.find(head).expect(head);
            src[start..start + src[start..].find(next).expect(next)].to_string()
        };
        assert!(
            section("pubasyncfnstart_backward_probe(", "letlegacy_keys=").contains(
                "ifletSome(entry)=drivers().get_mut(&owner_vk){make_seed(entry,owner_vk);"
            ),
            "an empty current key must make a running background merge seed"
        );
        assert!(
            section("pubasyncfnstart_rekey_merge(", "ifowed.is_empty()")
                .contains("ifseed{make_seed(entry,owner_vk);}"),
            "a seeding request must make a running background merge seed"
        );
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
                "ifletSome(room_data)=rooms.map.get_mut(&owner_vk){apply_rekey_finish(room_data,recovered,&satisfied,owner_vk,crate::util::get_current_system_time(),);}"
            ),
            "the merge and the marker update run together, in the deferred ROOMS write"
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

    /// The source of this file before its test module, comments and
    /// whitespace removed, for the wiring pins below.
    fn production_source() -> String {
        crate::util::strip_comments(
            include_str!("backward_probe.rs")
                .split("mod tests {")
                .next()
                .unwrap(),
        )
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
    }

    /// The real adapter, end to end through a Dioxus runtime: a background
    /// merge (the retry, or the one-time merge of #771) asks only
    /// the owed key, merges what it returns into ROOMS, clears the marker,
    /// drops its driver entry, and runs at most once per page load.
    #[test]
    fn a_background_rekey_merge_runs_once_per_load_and_asks_only_owed_keys() {
        use dioxus::prelude::*;
        let sk = SigningKey::from_bytes(&[95u8; 32]);
        let vk = sk.verifying_key();
        let legacy = owner_vk_to_legacy_contract_keys(&vk);
        assert!(
            legacy.len() > 2,
            "fixture: the registry has older generations"
        );
        // Owed: a generation two releases back. A walk over the skipped ones
        // would ask legacy[0] first.
        let owed_key = legacy[2];
        let owed_id = *owed_key.id();
        let local = room_with_messages(&sk, &["m1", "m2"]);
        let old_state = room_with_messages(&sk, &["m1", "m2", "m3"]);

        let dom = VirtualDom::new(|| rsx! {});
        dom.in_scope(ScopeId::ROOT, || {
            futures::executor::block_on(async {
                let mut room = room_data(&sk, local.clone());
                room.rekey_merge.owed = vec![owed_key];
                ROOMS.with_mut(|rooms| rooms.map.insert(vk, room));

                assert_eq!(
                    start_rekey_merge(vk, vec![owed_id], local.clone(), false).await,
                    RekeyStart::Started
                );
                assert!(routes().contains_key(&owed_id), "the owed key is asked");
                assert!(
                    !routes().contains_key(legacy[0].id()),
                    "a background merge does not walk the skipped generations"
                );
                assert_eq!(
                    start_rekey_merge(vk, vec![owed_id], local.clone(), false).await,
                    RekeyStart::AlreadyRunning
                );

                assert!(deliver_probe_response(owed_id, &encode(&old_state)).await);
                assert!(
                    drivers().get(&vk).is_none(),
                    "the driver entry is removed once the walk is done"
                );
                let rooms = ROOMS.read();
                let room = &rooms.map[&vk];
                assert_eq!(texts(&room.room_state), strs(&["m1", "m2", "m3"]));
                assert!(
                    room.rekey_merge.owed.is_empty(),
                    "the merge is no longer owed"
                );
                drop(rooms);

                assert_eq!(
                    start_rekey_merge(vk, vec![owed_id], local, false).await,
                    RekeyStart::AlreadyAttempted,
                    "at most one merge per owner per page load"
                );
            })
        });
    }

    /// Only the owed keys are asked by a background merge, in registry order;
    /// an unregistered owed key is asked last.
    #[test]
    fn owed_candidates_are_only_the_owed_keys_newest_first() {
        let legacy = vec![id(30), id(29), id(28), id(27)];
        assert_eq!(owed_candidates(&legacy, &[id(28)]), vec![id(28)]);
        assert_eq!(
            owed_candidates(&legacy, &[id(27), id(29)]),
            vec![id(29), id(27)]
        );
        assert_eq!(
            owed_candidates(&legacy, &[id(99), id(28)]),
            vec![id(28), id(99)]
        );
    }

    /// Two owed keys, only one answers: only that one comes off the marker, and
    /// the walk counts as unfinished.
    #[test]
    fn a_partial_rekey_finish_keeps_the_keys_that_did_not_answer() {
        let sk = SigningKey::from_bytes(&[96u8; 32]);
        let mut room = room_data(&sk, room_with_messages(&sk, &["m1"]));
        room.rekey_merge.owed = vec![contract_key_with_id(97), contract_key_with_id(98)];
        assert!(apply_rekey_finish(
            &mut room,
            Some(room_with_messages(&sk, &["m1", "m2"])),
            &[id(97)],
            sk.verifying_key(),
            std::time::SystemTime::now()
        ));
        assert_eq!(room.rekey_merge.owed, vec![contract_key_with_id(98)]);
        assert_eq!(room.rekey_merge.unfinished_loads, 1);
        assert_eq!(texts(&room.room_state), strs(&["m1", "m2"]));
    }

    /// A key that never returns the room is given up only once it has failed
    /// on `MAX_UNFINISHED_REKEY_LOADS` loads AND `REKEY_GIVE_UP_AFTER` has
    /// passed since the room first owed it: many quick reloads, or a few loads
    /// spread over days, are not enough on their own. A finished walk resets.
    #[test]
    fn a_key_that_never_answers_is_given_up_after_enough_loads_and_time() {
        use std::time::{Duration, SystemTime};
        let sk = SigningKey::from_bytes(&[99u8; 32]);
        let vk = sk.verifying_key();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let owed = || vec![contract_key_with_id(100)];
        let mut room = room_data(&sk, room_with_messages(&sk, &["m1"]));

        // Many loads in quick succession: still owed.
        room.rekey_merge.owed = owed();
        for load in 1..=MAX_UNFINISHED_REKEY_LOADS * 3 {
            // The first walk at t0 starts the clock.
            let now = t0 + Duration::from_secs(u64::from(load - 1) * 60);
            assert!(apply_rekey_finish(&mut room, None, &[], vk, now));
            assert_eq!(room.rekey_merge.owed, owed(), "load {load}: too soon");
        }
        // The same count, a moment short of the window: still owed.
        let almost = t0 + REKEY_GIVE_UP_AFTER - Duration::from_secs(1);
        assert!(apply_rekey_finish(&mut room, None, &[], vk, almost));
        assert_eq!(room.rekey_merge.owed, owed());
        // Window passed: given up, and the bookkeeping resets.
        assert!(apply_rekey_finish(
            &mut room,
            None,
            &[],
            vk,
            t0 + REKEY_GIVE_UP_AFTER
        ));
        assert!(room.rekey_merge.owed.is_empty(), "given up");
        assert_eq!(room.rekey_merge.unfinished_loads, 0);
        assert_eq!(room.rekey_merge.first_owed_at, None);

        // A few loads spread over a long time: still owed.
        room.rekey_merge.owed = owed();
        for load in 0..MAX_UNFINISHED_REKEY_LOADS - 1 {
            let now = t0 + REKEY_GIVE_UP_AFTER * (load + 1);
            assert!(apply_rekey_finish(&mut room, None, &[], vk, now));
            assert_eq!(room.rekey_merge.owed, owed(), "too few loads");
        }

        // A walk that finishes resets everything.
        assert!(apply_rekey_finish(
            &mut room,
            Some(room_with_messages(&sk, &["m1"])),
            &[id(100)],
            vk,
            t0
        ));
        assert!(room.rekey_merge.owed.is_empty());
        assert_eq!(room.rekey_merge.unfinished_loads, 0);
        assert_eq!(room.rekey_merge.first_owed_at, None);
    }

    /// An owed key answering with an empty (placeholder) state or with bytes
    /// that are not a room state has not returned the room. Another owed key
    /// returns real state in the same walk, so the merge itself lands: only
    /// that key is satisfied, and the one that answered junk stays owed.
    #[test]
    fn an_owed_key_answering_empty_or_garbage_satisfies_nothing() {
        let sk = SigningKey::from_bytes(&[101u8; 32]);
        let local = room_with_messages(&sk, &["m1"]);
        for junk in [
            Reply::State(ChatRoomStateV1::default()),
            Reply::Bytes(vec![0xff; 16]),
        ] {
            let finish = walk_rekey(
                &sk,
                local.clone(),
                &[id(102), id(101)],
                vec![id(101), id(102)],
                vec![junk, Reply::State(room_with_messages(&sk, &["m1", "m2"]))],
            );
            assert!(finish.recovered.is_some(), "the real answer is merged");
            assert_eq!(finish.satisfied, vec![id(101)], "only the real answer");
            assert!(!finish.complete);
        }
    }

    /// The seed deadline lets go of the room's seed only for the walk it was
    /// armed for, only while that walk still holds the seed, and only once.
    #[test]
    fn a_seed_deadline_releases_only_its_own_seeding_walk() {
        let vk = SigningKey::from_bytes(&[103u8; 32]).verifying_key();
        let entry = ProbeEntry::new(
            vk,
            ChatRoomStateV1::default(),
            vec![id(103)],
            ProbePurpose::RekeyMerge { seed: true },
            vec![id(103)],
        );
        let walk = entry.walk;
        drivers().insert(vk, entry);
        assert!(
            !take_seed_deadline(vk, walk + 1_000_000),
            "another walk's deadline"
        );
        assert!(
            take_seed_deadline(vk, walk),
            "the walk still holds the seed"
        );
        assert_eq!(
            drivers().get(&vk).unwrap().purpose,
            ProbePurpose::RekeyMerge { seed: false },
            "the walk carries on in the background"
        );
        assert!(!take_seed_deadline(vk, walk), "only once");
        drivers().remove(&vk);
        assert!(
            !take_seed_deadline(vk, walk),
            "a finished walk seeded itself"
        );

        let recover = ProbeEntry::new(
            vk,
            ChatRoomStateV1::default(),
            vec![id(103)],
            ProbePurpose::Recover,
            Vec::new(),
        );
        let walk = recover.walk;
        drivers().insert(vk, recover);
        assert!(
            !take_seed_deadline(vk, walk),
            "a #292 probe is not bounded here"
        );
        drivers().remove(&vk);
    }

    /// Wiring pins for the bounded seed (#771 review, must-fix B): every
    /// way a walk comes to hold the seed arms the deadline, and the deadline
    /// seeds through the ordinary seed path.
    #[test]
    fn every_seeding_walk_arms_the_seed_deadline() {
        let src = production_source();
        let section = |head: &str, next: &str| {
            let start = src.find(head).expect(head);
            src[start..start + src[start..].find(next).expect(next)].to_string()
        };
        let start = section(
            "pubasyncfnstart_rekey_merge(",
            "pubasyncfnstart_backward_probe(",
        );
        let arm = start
            .find("ifseed{arm_seed_deadline(owner_vk,walk);}")
            .expect("a seeding walk arms its deadline");
        let pump = start.find("pump_probe(owner_vk).await;").expect("pumps");
        assert!(arm < pump, "armed before the first GET goes out");
        assert!(
            start.contains("letcandidates=ifseed{rekey_candidates(&legacy,&owed)}else{owed_candidates(&legacy,&owed)};"),
            "a background merge asks only the owed keys"
        );
        assert!(
            section("fnmake_seed(", "fntake_seed_deadline(").contains("ifupgrade_to_seed(entry){")
                && section("fnmake_seed(", "fntake_seed_deadline(")
                    .contains("arm_seed_deadline(owner_vk,entry.walk);"),
            "a background walk told to seed arms its deadline too"
        );
        let deadline = section("fnarm_seed_deadline(", "pubasyncfnstart_rekey_merge(");
        let take = deadline
            .find("iftake_seed_deadline(owner_vk,walk){")
            .expect("the deadline acts only if the walk still holds the seed");
        let flag = deadline
            .find("sync_info.require_seed_put(&owner_vk)")
            .expect("forces the PUT route");
        let put = deadline.find("request_seed_put(owner_vk)").expect("seeds");
        assert!(take < flag && flag < put);
    }

    /// Wiring pin: the walk's terminal step plans the re-key finish from the
    /// entry's OWN owed keys, hits and fold flag, and applies that plan.
    #[test]
    fn pump_probe_plans_the_rekey_finish_from_its_own_entry() {
        let src = production_source();
        let start = src.find("asyncfnpump_probe(").expect("fn exists");
        let body =
            &src[start..start + src[start..].find("fnfinish_rekey_merge(").expect("next fn")];
        assert!(
            body.contains(
                "ifletProbePurpose::RekeyMerge{seed}=entry.purpose{ifletSome(outcome)=outcome{letplan=plan_rekey_finish(outcome,&entry.owed,&entry.hits,entry.fold_failed.load(Ordering::Relaxed),);finish_rekey_merge(owner_vk,seed,plan);}return;}"
            ),
            "the re-key finish must be planned from this walk's entry and applied"
        );
    }
    /// freenet-migrate 0.6: an unknown newest generation does NOT halt River's
    /// recovery probe (`continue_past_unknown`), and the walk still stops at
    /// the first generation with real state.
    #[test]
    fn an_unknown_newest_generation_is_walked_past_to_real_state() {
        let (sk, vk) = owner(8);
        let mut driver = new_probe_driver(
            RiverProbeOps::new(vk),
            ChatRoomStateV1::default(),
            vec![id(30), id(29), id(28)],
            SelectionPolicy::NewestFirstWins,
        );
        let Step::Get(a) = driver.next_action() else {
            panic!()
        };
        driver.on_unknown(a);
        let Step::Get(b) = driver.next_action() else {
            panic!("the probe must walk past an unknown generation")
        };
        assert_eq!(b, id(29));
        driver.on_response(b, &encode(&signed_state(&sk)));
        assert_eq!(
            driver.next_action(),
            Step::Done,
            "the first generation with real state ends the walk; older ones are never read"
        );
        let ProbeFinish::Adopt(merged, source) = probe_finish(driver.take_outcome()) else {
            panic!("real state must be adopted");
        };
        assert_eq!(source, id(29));
        assert!(merged.configuration.verify_signature(&vk).is_ok());
    }

    /// A typed `NotFound` is a positive absence (`on_absent`): an all-absent
    /// walk ends in a clean `SeedLocal`, not `Indeterminate`.
    #[test]
    fn an_all_absent_recovery_walk_seeds_cleanly() {
        let (sk, vk) = owner(9);
        let mut driver = new_probe_driver(
            RiverProbeOps::new(vk),
            signed_state(&sk),
            vec![id(41), id(40)],
            SelectionPolicy::NewestFirstWins,
        );
        for _ in 0..2 {
            let Step::Get(x) = driver.next_action() else {
                panic!()
            };
            driver.on_absent(x);
        }
        assert_eq!(driver.next_action(), Step::Done);
        assert!(matches!(
            driver.take_outcome(),
            Some(Outcome::SeedLocal { .. })
        ));
    }

    /// A taken outcome does nothing (a duplicate pump).
    #[test]
    fn a_taken_outcome_neither_adopts_nor_seeds() {
        assert!(matches!(probe_finish(None), ProbeFinish::Nothing));
    }
}
