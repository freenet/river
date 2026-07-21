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

use crate::components::app::freenet_api::constants::REPUT_DELAY_MS;
use crate::components::app::freenet_api::response_handler::get_response::{
    adopt_recovered_probe_state, merge_room_states, seed_current_key_with_local,
};
use crate::components::app::sync_info::SYNC_INFO;
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
use std::collections::HashMap;
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

/// One in-flight [`ProbeDriver`] per owner. The driver owns the sans-IO probe
/// decision state; this module only pumps I/O through it. Deduplicating
/// concurrent probes for one owner is our job (see [`start_backward_probe`]).
static PROBE_DRIVERS: LazyLock<Mutex<HashMap<VerifyingKey, ProbeDriver<RiverProbeOps>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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
fn drivers() -> MutexGuard<'static, HashMap<VerifyingKey, ProbeDriver<RiverProbeOps>>> {
    PROBE_DRIVERS.lock().unwrap_or_else(|e| e.into_inner())
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
    if drivers().contains_key(&owner_vk) {
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
    drivers().insert(owner_vk, driver);
    pump_probe(owner_vk).await;
    true
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
        if let Some(driver) = drivers.get_mut(&owner_vk) {
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
        let Some(driver) = drivers.get_mut(&owner_vk) else {
            // Driver already finished and was removed — nothing to pump.
            return;
        };
        driver.next_action()
    };

    match step {
        Step::Get(id) => fire_probe_get(owner_vk, id),
        Step::Done => {
            let outcome = drivers().get_mut(&owner_vk).and_then(|d| d.take_outcome());
            // Probe finished — drop the driver so a future probe for this owner
            // can start fresh, and so the dedup guard reads "not in flight".
            drivers().remove(&owner_vk);
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
                "Backward-probe GET for {id} timed out after {}s — treating the legacy \
                 generation as absent and advancing",
                PROBE_GET_TIMEOUT.as_secs()
            );
            if let Some(driver) = drivers().get_mut(&owner_vk) {
                driver.on_timeout(id);
            }
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
        // process-global PROBE_ROUTES map under parallel `cargo test`.
        let newest = id(70);
        let older = id(71);

        // Probe 1 fired `newest` with token t1 and has since COMPLETED (its
        // route was removed on completion), but its watchdog W1 is still armed,
        // holding t1.
        let t1 = next_fire_token();

        // Probe 2 then starts for the same owner and re-fires the SAME `newest`
        // id with a fresh token t2, and is sitting on it as its outstanding GET.
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

        // W1 fires late. Its token (t1) no longer matches `newest`'s route (t2),
        // so it must NOT claim the route and must NOT call on_timeout on probe 2.
        assert!(
            !claim_route_if_current(newest, vk, t1),
            "a stale watchdog (old token) must not claim a later probe's reused route"
        );
        assert_eq!(
            routes().get(&newest),
            Some(&(vk, t2)),
            "probe 2's route must survive the stale watchdog"
        );
        assert_eq!(
            probe2.next_action(),
            Step::Get(newest),
            "probe 2 must still probe its newest candidate — no rollback"
        );

        // Sanity: probe 2's OWN watchdog (token t2) DOES claim the route.
        assert!(
            claim_route_if_current(newest, vk, t2),
            "the current fire's watchdog claims its own route"
        );
        assert!(
            routes().remove(&newest).is_none(),
            "the matching claim already removed the route"
        );
    }
}
