//! The per-page-load driver for freenet-migrate's completeness journal
//! (freenet/river#763).
//!
//! `delegate_migration.rs` holds River's adapters (reader, writer, the journal's
//! storage). This module decides WHEN each journal operation runs, serialises
//! them, and turns their answers into the two decisions River makes from them:
//! whether the walk (and the sweep's recovery) must run, and whether the
//! identity-import gate may open.
//!
//! # What runs, and when
//!
//! * **`begin`, on every page load**, as soon as the current delegate has
//!   answered its room listing ([`start_journal_load`]), and lazily before any
//!   sweep merge if that comes first ([`gate_sweep_import`]). It starts the
//!   journal (read off the markers) the first time, and on later loads reopens a
//!   flag another tab's store overwrote.
//! * **The journaled walk, while `begin`'s status is incomplete**, whatever the
//!   load plan: after an interrupted per-room write (#760) the current delegate
//!   is no longer empty and the plan becomes `PerRoom`, so the walk can no longer
//!   hang off the `ProbeLegacy` sweep. Rate-limited by [`walk_decision`]
//!   (freenet/river#761) only while every pending predecessor is consistently not
//!   installed and nothing is mid-import.
//! * **`note_app_import_started` before every sweep merge** (`response_handler`):
//!   [`gate_sweep_import`] decides merge / merge unflagged / skip.
//! * **The walk again after a sweep flag**, once the load is quiet, so the
//!   sweep's import is resolved in the same session ([`poke_walk`]).
//!
//! Every journal operation is a read-merge-write of one stored value, and the
//! request registry holds a single waiter per correlation key, so they run one
//! at a time ([`JOURNAL_OPS`]).
//!
//! # Decisions come from the status the last call returned
//!
//! Never from a raw `journal_status` read, which reads no markers and can say
//! `Complete` over a partial copy another tab left. [`JournalSession`] keeps the
//! last status `begin` or the walk returned, plus whether one is running.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{LazyLock, Mutex};

use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::*;
use freenet_migrate::{DelegateLineageEntry, JournalStatus};
use freenet_stdlib::prelude::DelegateKey;

use super::delegate_migration::{
    begin_with, log_migration_report, note_sweep_import_with, read_walk_cadence,
    river_delegate_lineage, store_walk_cadence, walk_with, LiveImportSeam, NodeDelegateChannel,
    SweepWrite,
};
use crate::components::app::chat_delegate;

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

/// What this page load knows about the journal. Read by the identity-import
/// gate ([`import_gate`]); written only by the operations below.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct JournalSession {
    /// A `begin` succeeded on this page load.
    pub(crate) begun: bool,
    /// The status the last `begin` or walk returned (`None` before either has).
    pub(crate) status: Option<JournalStatus>,
    /// The last `begin` failed and none has succeeded since.
    pub(crate) begin_failed: bool,
    /// A walk is requested or running.
    pub(crate) walk_in_flight: bool,
    /// The last walk ran but its status is unknown (refused, or its final store
    /// failed): possibly incomplete.
    pub(crate) walk_status_unknown: bool,
    /// The sweep flagged an import after `status` was computed, so `status`
    /// does not know about it yet.
    pub(crate) sweep_flagged_since_status: bool,
}

impl JournalSession {
    /// `begin` succeeded and returned `status`.
    pub(crate) fn on_begin_ok(&mut self, status: JournalStatus) {
        self.begun = true;
        self.begin_failed = false;
        self.status = Some(status);
        self.sweep_flagged_since_status = false;
    }

    /// `begin` failed. Only a load that has never begun is "failed": a later
    /// failed re-begin leaves the last good answer standing.
    pub(crate) fn on_begin_err(&mut self) {
        self.begin_failed = !self.begun;
    }

    /// A walk finished and returned `status` (`None`: it was refused, or its
    /// final store failed, so the journal's state is unknown).
    pub(crate) fn on_walk_done(&mut self, status: Option<JournalStatus>) {
        self.walk_status_unknown = status.is_none();
        if let Some(status) = status {
            self.begun = true;
            self.begin_failed = false;
            self.status = Some(status);
            // Journal operations are serialised, so every sweep flag raised
            // before this walk took the lock is in the status it returned.
            self.sweep_flagged_since_status = false;
        }
    }

    /// What [`interrupted_import_for_recovery`] can answer now; `None` keeps it
    /// waiting for `begin`.
    pub(crate) fn recovery_answer(&self) -> Option<bool> {
        if let Some(status) = &self.status {
            return Some(status.interrupted_import() || self.sweep_flagged_since_status);
        }
        if self.begin_failed {
            return Some(true);
        }
        None
    }
}

static SESSION: LazyLock<Mutex<JournalSession>> =
    LazyLock::new(|| Mutex::new(JournalSession::default()));

fn session() -> std::sync::MutexGuard<'static, JournalSession> {
    SESSION.lock().unwrap_or_else(|p| p.into_inner())
}

/// Serialises every journal operation of this page load (`begin`, the sweep's
/// `note_app_import_started`, the walk, the cadence counter). Each is a
/// read-merge-write of the whole journal, which protects against another tab
/// but not against an interleaving inside this one, and the request registry
/// would hand one call's reply to another's waiter.
static JOURNAL_OPS: LazyLock<futures::lock::Mutex<()>> =
    LazyLock::new(|| futures::lock::Mutex::new(()));

/// The lineage every journal operation is given: the generated legacy registry,
/// oldest first, generation = registry index (the sweep's merge-rank scale).
static LINEAGE: LazyLock<Vec<DelegateLineageEntry>> =
    LazyLock::new(|| river_delegate_lineage(chat_delegate::legacy_delegate_pairs()));

/// The user chose to import an identity despite a journal that does not read
/// clean (see [`import_gate`]). Per page load, never stored.
static GATE_OVERRIDE: AtomicBool = AtomicBool::new(false);

/// Bumped whenever an input of [`current_import_gate`] changes, so a component
/// that reads it re-renders (the session itself is not a signal).
pub static JOURNAL_GATE_VERSION: GlobalSignal<u64> = Global::new(|| 0);

fn bump_gate_version() {
    // Natively `defer` runs its closure at once, and a `GlobalSignal` write
    // outside a Dioxus runtime panics, so the native tests of the driver skip
    // the re-render nudge (nothing renders there).
    #[cfg(not(test))]
    crate::util::defer(|| {
        *JOURNAL_GATE_VERSION.write() += 1;
    });
}

fn update_session(f: impl FnOnce(&mut JournalSession)) {
    f(&mut session());
    bump_gate_version();
}

// ---------------------------------------------------------------------------
// The identity-import gate
// ---------------------------------------------------------------------------

/// What the journal says about importing an identity right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImportGate {
    /// Nothing the journal knows of stands in the way.
    Open,
    /// Not known yet: `begin` has not answered, or a walk is running. Clears by
    /// itself within the load.
    Pending,
    /// The current delegate may not hold everything the user had: an import is
    /// interrupted, items were permanently rejected, or the journal could not be
    /// read or recorded. The user may override it.
    Blocked,
}

/// The identity-import gate (freenet/river#414, #763), composed from:
///
/// * a walk in flight, or a status not yet read → [`ImportGate::Pending`];
/// * `begin` failing, or a walk whose status is `None` → blocked: nothing
///   durable says what the successor holds;
/// * `interrupted_import()` on the status the last call returned, or a sweep
///   flag raised since → blocked: the successor may hold a partial copy;
/// * `rejected_items() > 0` → blocked: data the import path refused is not in
///   the successor until a fix raises `RIVER_IMPORT_REVISION`.
///
/// A silent predecessor that never started an import does NOT block: it keeps
/// the journal incomplete for as long as its generation is absent from this
/// node (most generations, on most nodes), so gating on `!is_complete()` would
/// block for good.
///
/// `override_on` (the user's "import anyway") ignores every blocking input but
/// never resolves a journal entry: a predecessor whose marker can never be read
/// keeps `interrupted_import()` up too, so resolving an entry would not clear
/// it (until freenet-migrate#39 gives a real way to give up on one). It does not
/// skip the transient inputs, which clear within the load.
pub(crate) fn import_gate(s: &JournalSession, override_on: bool) -> ImportGate {
    if s.walk_in_flight || (s.status.is_none() && !s.begin_failed) {
        return ImportGate::Pending;
    }
    let blocked = s.begin_failed
        || s.walk_status_unknown
        || s.sweep_flagged_since_status
        || s.status
            .as_ref()
            .is_some_and(|st| st.interrupted_import() || st.rejected_items() > 0);
    if blocked && !override_on {
        ImportGate::Blocked
    } else {
        ImportGate::Open
    }
}

/// The gate as it stands now.
pub(crate) fn current_import_gate() -> ImportGate {
    import_gate(&session(), GATE_OVERRIDE.load(Ordering::Relaxed))
}

/// The user's override: import an identity even though the journal blocks it.
pub(crate) fn override_import_gate() {
    warn!("identity import: the user overrode the migration-journal gate");
    GATE_OVERRIDE.store(true, Ordering::Relaxed);
    bump_gate_version();
}

// ---------------------------------------------------------------------------
// The walk decision (freenet/river#761 cadence)
// ---------------------------------------------------------------------------

/// A pending predecessor counts as consistently absent after this many
/// consecutive walks found it not installed.
pub(crate) const CADENCE_ABSENT_STREAK: u32 = 3;
/// While every pending predecessor is consistently absent, walk on one load in
/// this many.
pub(crate) const WALK_EVERY_N_LOADS: u32 = 10;

/// Whether this load runs the walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WalkDecision {
    /// Every predecessor is resolved: nothing to probe.
    NotNeeded,
    /// Run it.
    Run,
    /// Run it, and restart the cadence count.
    RunResetCadence,
    /// Skip it on this load, and record `skips` loads skipped so far.
    Skip {
        /// The new count of consecutive skipped loads.
        skips: u32,
    },
}

/// Whether the cadence counter must be read to decide the walk: only for a
/// status the rate limit can apply to (see [`walk_decision`]).
pub(crate) fn needs_cadence_counter(status: &JournalStatus) -> bool {
    !status.is_complete()
        && !status.interrupted_import()
        && status.pending_all_absent(CADENCE_ABSENT_STREAK)
}

/// Whether a sweep write the journal allowed raises the "flagged since the
/// status" bit and asks for a walk. Only a freshly FLAGGED import does: an
/// already-imported predecessor merges unflagged, and a skipped one writes
/// nothing.
pub(crate) fn sweep_write_raises_flag(write: SweepWrite) -> bool {
    write == SweepWrite::Merge
}

/// Decide the walk from the status `begin` returned (freenet/river#761).
///
/// `Complete` is out of reach while generations this node never had stay
/// pending, so the walk would otherwise re-probe all 29 generations on every
/// load for good. `pending_all_absent` is a RATE LIMIT only: while every pending
/// predecessor answered "not installed" on the last [`CADENCE_ABSENT_STREAK`]
/// walks and none is mid-import, the walk runs on one load in
/// [`WALK_EVERY_N_LOADS`], counted in River's own KV (`skips`: `None` when the
/// counter could not be read, which runs the walk). An interrupted import, or a
/// predecessor that is silent rather than typed-absent, always walks.
pub(crate) fn walk_decision(status: &JournalStatus, skips: Option<u32>) -> WalkDecision {
    if status.is_complete() {
        return WalkDecision::NotNeeded;
    }
    if status.interrupted_import() || !status.pending_all_absent(CADENCE_ABSENT_STREAK) {
        return WalkDecision::Run;
    }
    match skips {
        None => WalkDecision::Run,
        Some(n) if n.saturating_add(1) >= WALK_EVERY_N_LOADS => WalkDecision::RunResetCadence,
        Some(n) => WalkDecision::Skip { skips: n + 1 },
    }
}

// ---------------------------------------------------------------------------
// begin, on every page load
// ---------------------------------------------------------------------------

/// Once per page load, unless `begin` fails (then the next load attempt
/// retries).
static LOAD_ARMED: AtomicBool = AtomicBool::new(false);

/// Start this page load's journal work: `begin`, then the walk if the status
/// calls for it. Called once the current delegate has answered its room
/// listing, so it exists on the node. Idempotent within a page load.
pub(crate) fn start_journal_load() {
    if LOAD_ARMED.swap(true, Ordering::Relaxed) {
        return;
    }
    crate::util::safe_spawn_local(async {
        let Some(status) = begin_locked().await else {
            // Retry on the next load attempt (reconnect / Retry button).
            LOAD_ARMED.store(false, Ordering::Relaxed);
            return;
        };
        let decision = {
            let _ops = JOURNAL_OPS.lock().await;
            let channel = NodeDelegateChannel;
            let successor = chat_delegate::current_delegate_key();
            let skips = if needs_cadence_counter(&status) {
                read_walk_cadence(&channel, &successor).await.ok()
            } else {
                None
            };
            let decision = walk_decision(&status, skips);
            match decision {
                WalkDecision::Skip { skips } => {
                    match store_walk_cadence(&channel, &successor, skips).await {
                        Ok(()) => decision,
                        // A skip that is not counted could repeat for good.
                        Err(_) => WalkDecision::Run,
                    }
                }
                WalkDecision::RunResetCadence => {
                    let _ = store_walk_cadence(&channel, &successor, 0).await;
                    decision
                }
                _ => decision,
            }
        };
        info!("migration journal: walk decision for this load: {decision:?}");
        if matches!(decision, WalkDecision::Run | WalkDecision::RunResetCadence) {
            poke_walk();
        }
    });
}

/// Run `begin` under the journal lock and record its answer. `None` on failure.
async fn begin_locked() -> Option<JournalStatus> {
    let _ops = JOURNAL_OPS.lock().await;
    begin_unlocked().await
}

/// `begin` with the journal lock already held.
async fn begin_unlocked() -> Option<JournalStatus> {
    let result = begin_with(
        &NodeDelegateChannel,
        chat_delegate::current_delegate_key(),
        &LINEAGE,
    )
    .await;
    match result {
        Ok(status) => {
            info!(
                "migration journal: begin: {} (interrupted import: {}, rejected items: {})",
                if status.is_complete() {
                    "complete"
                } else {
                    "incomplete"
                },
                status.interrupted_import(),
                status.rejected_items()
            );
            let recorded = status.clone();
            update_session(move |s| s.on_begin_ok(recorded));
            Some(status)
        }
        Err(e) => {
            warn!("migration journal: begin failed ({e}); no migrated write will happen until it succeeds");
            update_session(JournalSession::on_begin_err);
            None
        }
    }
}

/// Whether the per-room load should re-run the sweep to recover rooms an
/// interrupted migration did not write (freenet/river#345 follow-up, #760).
/// Waits (bounded) for this load's `begin`. Unknown counts as interrupted: the
/// re-run's merges are ranked and tombstone-safe, and each is gated by the
/// journal, so an unneeded one costs requests, never data.
pub(crate) async fn interrupted_import_for_recovery() -> bool {
    const POLL_MS: u64 = 250;
    const MAX_MS: u64 = 30_000;
    let mut waited = 0;
    loop {
        if let Some(answer) = session().recovery_answer() {
            return answer;
        }
        if waited >= MAX_MS {
            return true;
        }
        crate::util::sleep(std::time::Duration::from_millis(POLL_MS)).await;
        waited += POLL_MS;
    }
}

// ---------------------------------------------------------------------------
// The sweep's merges
// ---------------------------------------------------------------------------

/// Decide a sweep merge of data read from the legacy delegate `responding`:
/// `begin` first if this page load has not run it yet (it must precede every
/// migrated write), then `note_app_import_started` with `items`. A flagged
/// import also asks for a walk, so the same session resolves it.
pub(crate) async fn gate_sweep_import(responding: &DelegateKey, items: Option<u32>) -> SweepWrite {
    let write = {
        let _ops = JOURNAL_OPS.lock().await;
        let begun = session().begun;
        if !begun && begin_unlocked().await.is_none() {
            return SweepWrite::Skip;
        }
        note_sweep_import_with(
            &NodeDelegateChannel,
            chat_delegate::current_delegate_key(),
            &LINEAGE,
            responding.bytes(),
            items,
        )
        .await
    };
    if sweep_write_raises_flag(write) {
        update_session(|s| s.sweep_flagged_since_status = true);
        poke_walk();
    }
    write
}

/// Gate attempts per sweep merge, and the waits between them. A journal round
/// trip that fails once (a slow node, a displaced waiter) usually succeeds on a
/// retry; skipping straight away would leave that predecessor to the walk
/// while older generations merge first (freenet/river#768 review A2).
const SWEEP_GATE_BACKOFF_MS: [u64; 2] = [1_000, 3_000];

/// Run `gate` until it allows the merge, at most `1 + backoff.len()` times,
/// waiting `backoff[i]` before retry `i + 1`. Generic so the retry shape is
/// natively testable.
pub(crate) async fn with_gate_retries<G, GF, S, SF>(
    mut gate: G,
    mut sleep: S,
    backoff: &[u64],
) -> SweepWrite
where
    G: FnMut() -> GF,
    GF: Future<Output = SweepWrite>,
    S: FnMut(u64) -> SF,
    SF: Future<Output = ()>,
{
    let mut write = gate().await;
    for ms in backoff {
        if write != SweepWrite::Skip {
            break;
        }
        sleep(*ms).await;
        write = gate().await;
    }
    write
}

/// `generation + 1` of the newest legacy generation whose data the sweep held
/// but the journal would not let it merge this page load (`0`: none).
static SWEEP_SKIPPED_NEWEST: AtomicU32 = AtomicU32::new(0);

/// Whether generation `generation` is OLDER than one the sweep had to skip
/// this page load (`skipped_marker` = that generation + 1). River migrates
/// under Union, so the walk would import it anyway; but the sweep merging an
/// older generation's copy and saving it while a newer one that answered with
/// data is held back is exactly the cross-load shadowing freenet-migrate#42
/// warns about. So the sweep leaves older generations to the walk too.
pub(crate) fn older_than_a_skipped_newer(generation: u32, skipped_marker: u32) -> bool {
    skipped_marker > generation.saturating_add(1)
}

/// The sweep's journal gate for data read from the legacy delegate
/// `responding` (freenet/river#763, #768 review A2): refused outright if a
/// newer generation was skipped this page load; otherwise
/// [`gate_sweep_import`], retried with bounded backoff. A final skip is
/// recorded so older generations are held back with it. The caller marks the
/// load's fetch failure on a skip, so the UI offers Retry rather than a false
/// "no rooms".
pub(crate) async fn gate_sweep_import_retrying(
    responding: &DelegateKey,
    items: Option<u32>,
) -> SweepWrite {
    let Some(generation) = LINEAGE
        .iter()
        .find(|e| e.delegate_key.as_slice() == responding.bytes())
        .map(|e| e.generation)
    else {
        return SweepWrite::Skip;
    };
    if older_than_a_skipped_newer(generation, SWEEP_SKIPPED_NEWEST.load(Ordering::Relaxed)) {
        warn!(
            "sweep: legacy gen {generation} held back: a newer generation's data was skipped \
             this load"
        );
        return SweepWrite::Skip;
    }
    let write = with_gate_retries(
        || gate_sweep_import(responding, items),
        |ms| crate::util::sleep(std::time::Duration::from_millis(ms)),
        &SWEEP_GATE_BACKOFF_MS,
    )
    .await;
    if write == SweepWrite::Skip {
        SWEEP_SKIPPED_NEWEST.fetch_max(generation.saturating_add(1), Ordering::Relaxed);
    }
    write
}

// ---------------------------------------------------------------------------
// The walk driver
// ---------------------------------------------------------------------------

/// A walk was requested and has not run yet.
static WALK_POKED: AtomicBool = AtomicBool::new(false);
/// The walk driver task is alive.
static WALK_DRIVER_RUNNING: AtomicBool = AtomicBool::new(false);
/// Walks this page load (bounded: [`MAX_WALKS_PER_LOAD`]).
static WALKS_THIS_LOAD: AtomicU32 = AtomicU32::new(0);

/// The load's own walk plus re-runs after sweep flags. A sweep flag after the
/// cap stays flagged in the journal, so the next load's `begin` reports it and
/// walks; nothing is lost by stopping here.
const MAX_WALKS_PER_LOAD: u32 = 4;

/// How long the walk waits for the sweep's load workers to settle before its
/// first round-trip, so the two do not contend for the same single-waiter
/// correlation slots. Bounded: a collision degrades to an unresolved
/// predecessor and a retry, never to a wrong write.
const WALK_QUIESCENCE_MAX_MS: u64 = 60_000;
const WALK_QUIESCENCE_POLL_MS: u64 = 500;

/// Ask for a journaled walk. One driver task runs walks one after another while
/// asked, so a reconnect or a burst of sweep flags never stacks concurrent
/// walks.
pub(crate) fn poke_walk() {
    WALK_POKED.store(true, Ordering::Relaxed);
    update_session(|s| s.walk_in_flight = true);
    if WALK_DRIVER_RUNNING.swap(true, Ordering::Relaxed) {
        return;
    }
    crate::util::safe_spawn_local(walk_driver());
}

/// What the walk driver reads from and does to its environment. The live
/// implementation is the node/Dioxus runtime ([`LiveWalkEnv`]); the native
/// tests substitute a scripted one, because the driver's branches (the
/// fetch-failure gate of freenet/river#595, the wait for the load to settle,
/// the walk cap, a poke landing mid-walk) are otherwise reachable only in a
/// browser.
pub(crate) trait WalkEnv {
    /// Load workers still in flight (the sweep and the room load).
    fn pending_load_workers(&self) -> u32;
    /// The current load attempt (a reconnect or Retry begins a new one).
    fn load_attempt(&self) -> u32;
    /// Whether load attempt `attempt` read the whole current room set and is
    /// still current (`chat_delegate::room_set_complete`).
    fn room_set_complete(&self, attempt: u32) -> bool;
    /// Wait `ms` milliseconds.
    fn sleep(&self, ms: u64) -> impl Future<Output = ()>;
    /// Run one journaled walk to completion on behalf of load attempt
    /// `attempt` (its seam re-checks the room set before every merge and its
    /// flush).
    fn run_walk(&self, attempt: u32) -> impl Future<Output = ()>;
}

struct LiveWalkEnv;

impl WalkEnv for LiveWalkEnv {
    fn pending_load_workers(&self) -> u32 {
        chat_delegate::pending_load_workers()
    }
    fn load_attempt(&self) -> u32 {
        chat_delegate::current_load_attempt()
    }
    fn room_set_complete(&self, attempt: u32) -> bool {
        chat_delegate::room_set_complete(attempt)
    }
    async fn sleep(&self, ms: u64) {
        crate::util::sleep(std::time::Duration::from_millis(ms)).await
    }
    async fn run_walk(&self, attempt: u32) {
        run_walk_locked(attempt).await
    }
}

async fn walk_driver() {
    walk_driver_with(&LiveWalkEnv).await
}

async fn walk_driver_with<E: WalkEnv>(env: &E) {
    loop {
        while WALK_POKED.swap(false, Ordering::Relaxed) {
            if WALKS_THIS_LOAD.load(Ordering::Relaxed) >= MAX_WALKS_PER_LOAD {
                warn!(
                    "migration journal: {MAX_WALKS_PER_LOAD} walks this page load; the next \
                     load continues"
                );
                break;
            }
            let attempt = env.load_attempt();
            let mut waited = 0u64;
            while env.pending_load_workers() != 0 && waited < WALK_QUIESCENCE_MAX_MS {
                env.sleep(WALK_QUIESCENCE_POLL_MS).await;
                waited += WALK_QUIESCENCE_POLL_MS;
            }
            let check = WalkCheck {
                settled: env.pending_load_workers() == 0,
                same_attempt: env.load_attempt() == attempt,
                room_set_complete: env.room_set_complete(attempt),
            };
            if !walk_may_run(check) {
                warn!(
                    "migration journal: not walking on this load attempt ({check:?}): the \
                     current room set is not known complete (freenet/river#595). The next \
                     load attempt retries."
                );
                // Let the next load attempt (a reconnect, or Retry) start again.
                // A gated attempt does not spend the per-load walk budget.
                LOAD_ARMED.store(false, Ordering::Relaxed);
                continue;
            }
            WALKS_THIS_LOAD.fetch_add(1, Ordering::Relaxed);
            env.run_walk(attempt).await;
        }
        WALK_DRIVER_RUNNING.store(false, Ordering::Relaxed);
        // A poke that landed after the last check found the driver still
        // running and spawned nothing: pick it up here.
        let resume = WALK_POKED.load(Ordering::Relaxed)
            && WALKS_THIS_LOAD.load(Ordering::Relaxed) < MAX_WALKS_PER_LOAD
            && !WALK_DRIVER_RUNNING.swap(true, Ordering::Relaxed);
        if !resume {
            break;
        }
    }
    update_session(|s| s.walk_in_flight = false);
}

/// What the driver established just before a walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WalkCheck {
    /// No load worker is still in flight (it did not just hit the wait cap).
    pub(crate) settled: bool,
    /// No reconnect or Retry began a new load attempt during the wait (which
    /// zeroes the worker count and the failure flag the wait relies on).
    pub(crate) same_attempt: bool,
    /// The attempt read the whole current room set and nothing failed
    /// (`chat_delegate::room_set_complete`).
    pub(crate) room_set_complete: bool,
}

/// Whether a requested walk may run now: only over a current room set this
/// attempt read in full. The walk merges older copies into the in-memory set
/// and re-saves it, and the save keeps the in-memory identity of a room whose
/// stored slot this load could not read, overwriting that slot
/// (freenet/river#595). Before #763 the walk ran only behind an empty current
/// delegate; on every load it must not run over an incomplete set. The
/// predecessors stay pending, so nothing is lost by waiting.
pub(crate) fn walk_may_run(check: WalkCheck) -> bool {
    check.settled && check.same_attempt && check.room_set_complete
}

/// One journaled walk under the journal lock, on behalf of load attempt
/// `attempt`, recording its status.
async fn run_walk_locked(attempt: u32) {
    let _ops = JOURNAL_OPS.lock().await;
    let (report, _seam) = walk_with(
        &NodeDelegateChannel,
        LiveImportSeam::new(attempt),
        chat_delegate::current_delegate_key(),
        &LINEAGE,
    )
    .await;
    log_migration_report(&report);
    let status = report.status.clone();
    update_session(move |s| s.on_walk_done(status));
}

#[cfg(test)]
mod tests {
    use super::*;
    use freenet_migrate::{
        begin_migration_journal, DelegateLineageEntry, MigrationJournal, PendingPredecessor,
    };

    fn incomplete(pending: Vec<PendingPredecessor>, rejected_items: u32) -> JournalStatus {
        JournalStatus::Incomplete {
            pending,
            rejected_items,
        }
    }

    /// Real statuses, produced by the crate (its status types are
    /// `#[non_exhaustive]`): a fresh journal over a 2-generation lineage is
    /// `Incomplete` with both pending, nothing started, streak 0.
    fn fresh_status() -> JournalStatus {
        let lineage: Vec<DelegateLineageEntry> =
            river_delegate_lineage(&[([1u8; 32], [2u8; 32]), ([3u8; 32], [4u8; 32])]);
        let journal = MigrationJournal::new_pending(&lineage);
        journal.status(
            &lineage,
            &freenet_migrate::JournalBinding::new(
                &super::super::delegate_migration::river_secret_policy(),
                0,
            ),
        )
    }

    fn known(status: JournalStatus) -> JournalSession {
        JournalSession {
            begun: true,
            status: Some(status),
            ..Default::default()
        }
    }

    #[test]
    fn the_gate_waits_for_the_first_status_and_for_a_running_walk() {
        assert_eq!(
            import_gate(&JournalSession::default(), false),
            ImportGate::Pending,
            "no status read yet: wait"
        );
        let mut s = known(JournalStatus::Complete { rejected_items: 0 });
        assert_eq!(import_gate(&s, false), ImportGate::Open);
        s.walk_in_flight = true;
        assert_eq!(import_gate(&s, false), ImportGate::Pending);
        assert_eq!(
            import_gate(&s, true),
            ImportGate::Pending,
            "the override does not skip a walk that is about to answer"
        );
    }

    /// A journal kept incomplete only by predecessors that never started an
    /// import (absent generations) must NOT block: that is most nodes, for good.
    #[test]
    fn an_incomplete_journal_with_nothing_started_does_not_block() {
        let status = fresh_status();
        assert!(!status.is_complete());
        assert!(!status.interrupted_import());
        assert_eq!(import_gate(&known(status), false), ImportGate::Open);
    }

    #[test]
    fn every_blocking_input_blocks_and_the_override_opens_each() {
        let mut cases: Vec<(&str, JournalSession)> = Vec::new();
        cases.push((
            "begin failed",
            JournalSession {
                begin_failed: true,
                ..Default::default()
            },
        ));
        let mut s = known(JournalStatus::Complete { rejected_items: 0 });
        s.walk_status_unknown = true;
        cases.push(("walk status unknown", s));
        let mut s = known(JournalStatus::Complete { rejected_items: 0 });
        s.sweep_flagged_since_status = true;
        cases.push(("sweep flagged since the status", s));
        cases.push((
            "rejected items",
            known(JournalStatus::Complete { rejected_items: 1 }),
        ));
        cases.push((
            "rejected items while incomplete",
            known(incomplete(vec![], 2)),
        ));
        cases.push((
            "unreadable journal (interrupted_import)",
            known(JournalStatus::Unreadable(
                freenet_migrate::JournalDecodeError::Corrupt("x".into()),
            )),
        ));
        for (what, s) in cases {
            assert_eq!(import_gate(&s, false), ImportGate::Blocked, "{what}");
            assert_eq!(import_gate(&s, true), ImportGate::Open, "{what} (override)");
        }
    }

    /// An interrupted import, as the crate itself reports one after a sweep
    /// flag: `begin` over a successor whose marker says an import started.
    #[test]
    fn an_interrupted_import_blocks() {
        let status = interrupted_status();
        assert!(status.interrupted_import());
        assert_eq!(
            import_gate(&known(status.clone()), false),
            ImportGate::Blocked
        );
        assert_eq!(import_gate(&known(status), true), ImportGate::Open);
    }

    /// A status with an interrupted import, produced by the real crate: `begin`
    /// over a successor whose in-progress marker for one predecessor says data.
    fn interrupted_status() -> JournalStatus {
        use super::super::delegate_migration::{pred_wip_marker_key, RiverSuccessorIo};
        use freenet_migrate::PRED_DONE_MARKER_VALUE_DATA;
        use freenet_stdlib::prelude::CodeHash;
        let lineage: Vec<DelegateLineageEntry> =
            river_delegate_lineage(&[([1u8; 32], [2u8; 32]), ([3u8; 32], [4u8; 32])]);
        let successor = DelegateKey::new([9u8; 32], CodeHash::new([8u8; 32]));
        let pred = DelegateKey::new([1u8; 32], CodeHash::new([2u8; 32]));
        let channel = super::super::delegate_migration::tests_support::MockChannel::default();
        channel.put(
            &successor,
            &pred_wip_marker_key(&pred),
            PRED_DONE_MARKER_VALUE_DATA,
        );
        let mut writer = RiverSuccessorIo::new(
            &channel,
            super::super::delegate_migration::NoImports,
            successor,
        );
        futures::executor::block_on(begin_migration_journal(
            &mut writer,
            &lineage,
            &super::super::delegate_migration::river_secret_policy(),
        ))
        .expect("begin")
    }

    #[test]
    fn walk_decision_runs_unless_complete_or_rate_limited() {
        assert_eq!(
            walk_decision(&JournalStatus::Complete { rejected_items: 0 }, None),
            WalkDecision::NotNeeded
        );
        // Nothing typed-absent yet: always walk, never read the counter.
        assert_eq!(walk_decision(&fresh_status(), Some(3)), WalkDecision::Run);
        assert_eq!(
            walk_decision(&interrupted_status(), Some(3)),
            WalkDecision::Run,
            "an interrupted import always walks"
        );
    }

    /// The cadence only ever rate-limits a status in which every pending
    /// predecessor is consistently absent; it never skips two loads in a row
    /// past `WALK_EVERY_N_LOADS`, and an unreadable counter walks.
    #[test]
    fn walk_decision_cadence_counts_to_n_then_walks() {
        let status = all_absent_status();
        assert!(status.pending_all_absent(CADENCE_ABSENT_STREAK));
        assert_eq!(walk_decision(&status, None), WalkDecision::Run);
        let mut skips = 0;
        let mut walked_at = None;
        for load in 1..=WALK_EVERY_N_LOADS {
            match walk_decision(&status, Some(skips)) {
                WalkDecision::Skip { skips: n } => {
                    assert_eq!(n, skips + 1);
                    skips = n;
                }
                WalkDecision::RunResetCadence => {
                    walked_at = Some(load);
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(walked_at, Some(WALK_EVERY_N_LOADS), "one walk in N loads");
    }

    /// A status where every pending predecessor answered "not installed" on
    /// `CADENCE_ABSENT_STREAK` walks, produced by the real crate (absent
    /// streaks are only reachable through journaled walks).
    fn all_absent_status() -> JournalStatus {
        super::super::delegate_migration::tests_support::all_absent_status(CADENCE_ABSENT_STREAK)
    }

    /// freenet/river#595: never walk over a room set that failed to load.
    /// The driver must consult the gate before every walk; pinned in source
    /// because the driver is wasm-only.
    #[test]
    fn the_walk_never_runs_over_an_incomplete_room_set() {
        let ok = WalkCheck {
            settled: true,
            same_attempt: true,
            room_set_complete: true,
        };
        assert!(walk_may_run(ok));
        for bad in [
            WalkCheck {
                settled: false,
                ..ok
            },
            WalkCheck {
                same_attempt: false,
                ..ok
            },
            WalkCheck {
                room_set_complete: false,
                ..ok
            },
        ] {
            assert!(!walk_may_run(bad), "{bad:?}");
        }
    }

    // ---------------- probes added by the testing review of PR #768 ----------------

    /// The session transitions the driver applies (wasm-only code, so the
    /// transitions are pure methods the native tests can reach).
    #[test]
    fn session_transitions_keep_the_gate_honest() {
        // A failed first begin blocks; a failed RE-begin leaves the good answer.
        let mut s = JournalSession::default();
        s.on_begin_err();
        assert!(s.begin_failed);
        assert_eq!(import_gate(&s, false), ImportGate::Blocked);
        s.on_begin_ok(JournalStatus::Complete { rejected_items: 0 });
        assert!(!s.begin_failed && s.begun);
        assert_eq!(import_gate(&s, false), ImportGate::Open);
        s.on_begin_err();
        assert!(
            !s.begin_failed,
            "a failed re-begin must not discard the last good answer"
        );
        assert_eq!(import_gate(&s, false), ImportGate::Open);

        // A sweep flag blocks until a begin or a walk returns a status that
        // knows about it.
        s.sweep_flagged_since_status = true;
        assert_eq!(import_gate(&s, false), ImportGate::Blocked);
        let mut after_begin = s.clone();
        after_begin.on_begin_ok(JournalStatus::Complete { rejected_items: 0 });
        assert_eq!(import_gate(&after_begin, false), ImportGate::Open);
        let mut after_walk = s.clone();
        after_walk.on_walk_done(Some(JournalStatus::Complete { rejected_items: 0 }));
        assert_eq!(import_gate(&after_walk, false), ImportGate::Open);

        // A walk with no status (refused, or its store failed) blocks and keeps
        // the old status; the next walk with a status clears it.
        let mut w = known(JournalStatus::Complete { rejected_items: 0 });
        w.on_walk_done(None);
        assert!(w.walk_status_unknown && w.status.is_some());
        assert_eq!(import_gate(&w, false), ImportGate::Blocked);
        w.on_walk_done(Some(JournalStatus::Complete { rejected_items: 0 }));
        assert!(!w.walk_status_unknown);
        assert_eq!(import_gate(&w, false), ImportGate::Open);

        // A walk's status replaces begin's, including rejected items.
        w.on_walk_done(Some(JournalStatus::Complete { rejected_items: 2 }));
        assert_eq!(import_gate(&w, false), ImportGate::Blocked);
    }

    /// `interrupted_import_for_recovery`'s decision per session state: the
    /// recovery re-run is the only path that restores rooms an interrupted
    /// migration never wrote, so "unknown" must lean to recovering.
    #[test]
    fn recovery_answers_lean_to_recovering() {
        assert_eq!(
            JournalSession::default().recovery_answer(),
            None,
            "wait for begin"
        );
        let mut failed = JournalSession::default();
        failed.on_begin_err();
        assert_eq!(failed.recovery_answer(), Some(true));
        assert_eq!(
            known(JournalStatus::Complete { rejected_items: 0 }).recovery_answer(),
            Some(false)
        );
        let mut flagged = known(JournalStatus::Complete { rejected_items: 0 });
        flagged.sweep_flagged_since_status = true;
        assert_eq!(
            flagged.recovery_answer(),
            Some(true),
            "a flag the status does not know yet"
        );
        assert_eq!(known(interrupted_status()).recovery_answer(), Some(true));
    }

    #[test]
    fn only_a_freshly_flagged_sweep_import_raises_the_flag() {
        assert!(sweep_write_raises_flag(SweepWrite::Merge));
        assert!(!sweep_write_raises_flag(SweepWrite::MergeUnflagged));
        assert!(!sweep_write_raises_flag(SweepWrite::Skip));
    }

    /// The counter is read only for a status the rate limit can apply to.
    #[test]
    fn the_cadence_counter_is_read_only_when_it_can_apply() {
        assert!(needs_cadence_counter(&all_absent_status()));
        assert!(!needs_cadence_counter(&JournalStatus::Complete {
            rejected_items: 0
        }));
        assert!(
            !needs_cadence_counter(&fresh_status()),
            "nothing typed-absent yet"
        );
        assert!(!needs_cadence_counter(&interrupted_status()));
    }

    // ---------------- the walk driver, against a scripted environment ----------------

    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    /// The driver's state is process-global (like the page load it models), so
    /// the driver tests run one at a time and start from a fresh load.
    static DRIVER_TESTS: Mutex<()> = Mutex::new(());

    fn fresh_load() -> std::sync::MutexGuard<'static, ()> {
        let guard = DRIVER_TESTS.lock().unwrap_or_else(|p| p.into_inner());
        WALK_POKED.store(false, Ordering::Relaxed);
        WALK_DRIVER_RUNNING.store(false, Ordering::Relaxed);
        WALKS_THIS_LOAD.store(0, Ordering::Relaxed);
        LOAD_ARMED.store(true, Ordering::Relaxed);
        *session() = JournalSession::default();
        guard
    }

    #[derive(Default)]
    struct ScriptedEnv {
        /// `pending_load_workers` answers these in turn, then 0.
        workers: RefCell<VecDeque<u32>>,
        /// `pending_load_workers` answers this forever when set.
        stuck_workers: Cell<Option<u32>>,
        /// The room set is NOT complete for the attempt (a failed slot, a
        /// failed `rooms_meta`, any fetch failure).
        incomplete: Cell<bool>,
        attempt: Cell<u32>,
        /// A reconnect/Retry begins a new attempt during the wait.
        new_attempt_during_wait: Cell<bool>,
        /// Called at the start of each walk (a sweep poking mid-walk, say).
        during_walk: RefCell<Option<Box<dyn Fn()>>>,
        log: RefCell<Vec<&'static str>>,
    }

    impl WalkEnv for ScriptedEnv {
        fn pending_load_workers(&self) -> u32 {
            if let Some(n) = self.stuck_workers.get() {
                return n;
            }
            self.workers.borrow_mut().pop_front().unwrap_or(0)
        }
        fn load_attempt(&self) -> u32 {
            self.attempt.get()
        }
        fn room_set_complete(&self, attempt: u32) -> bool {
            attempt == self.attempt.get() && !self.incomplete.get()
        }
        async fn sleep(&self, _ms: u64) {
            self.log.borrow_mut().push("sleep");
            if self.new_attempt_during_wait.get() {
                self.attempt.set(self.attempt.get() + 1);
            }
        }
        async fn run_walk(&self, _attempt: u32) {
            self.log.borrow_mut().push("walk");
            if let Some(f) = self.during_walk.borrow_mut().take() {
                f();
            }
        }
    }

    fn walks(env: &ScriptedEnv) -> usize {
        env.log.borrow().iter().filter(|e| **e == "walk").count()
    }

    /// Ask for a walk and run the driver to the end (the native `poke_walk`
    /// raises the flag; the spawn it would make is a no-op off the browser).
    fn poke_and_drive(env: &ScriptedEnv) {
        poke_walk();
        futures::executor::block_on(walk_driver_with(env));
    }

    #[test]
    fn the_driver_walks_once_and_clears_the_in_flight_flag() {
        let _g = fresh_load();
        let env = ScriptedEnv::default();
        poke_walk();
        assert!(
            session().walk_in_flight,
            "a requested walk holds the gate Pending"
        );
        futures::executor::block_on(walk_driver_with(&env));
        assert_eq!(walks(&env), 1);
        assert!(!session().walk_in_flight);
        assert!(LOAD_ARMED.load(Ordering::Relaxed));
        assert!(!WALK_DRIVER_RUNNING.load(Ordering::Relaxed));
    }

    /// freenet/river#595: never walk over a room set that failed to load, and
    /// let the next load attempt start the journal work again. The scrape
    /// test next to `walk_may_run` is satisfied by a driver whose gate is
    /// disabled; this one is not.
    #[test]
    fn the_driver_does_not_walk_over_a_failed_room_load_and_rearms_the_load() {
        let _g = fresh_load();
        let env = ScriptedEnv::default();
        env.incomplete.set(true);
        poke_and_drive(&env);
        assert_eq!(walks(&env), 0, "no walk over an incomplete room set");
        assert!(
            !LOAD_ARMED.load(Ordering::Relaxed),
            "the next load attempt must be able to start the journal work again"
        );
        assert!(!session().walk_in_flight, "the gate must not stay Pending");
    }

    /// freenet/river#253 ordering: the walk waits for the load's workers to
    /// settle, and only then walks.
    #[test]
    fn the_driver_waits_for_the_load_workers_before_walking() {
        let _g = fresh_load();
        let env = ScriptedEnv::default();
        *env.workers.borrow_mut() = VecDeque::from([2, 1]);
        poke_and_drive(&env);
        assert_eq!(*env.log.borrow(), vec!["sleep", "sleep", "walk"]);
    }

    /// The wait is bounded, and a load still pending at the cap ABORTS the
    /// walk (and re-arms the load) rather than walking over a room set still
    /// being read (freenet/river#763 review A1, #595).
    #[test]
    fn a_load_still_pending_at_the_wait_cap_aborts_the_walk() {
        let _g = fresh_load();
        let env = ScriptedEnv::default();
        env.stuck_workers.set(Some(1));
        poke_and_drive(&env);
        let sleeps = env.log.borrow().iter().filter(|e| **e == "sleep").count() as u64;
        assert_eq!(sleeps, WALK_QUIESCENCE_MAX_MS / WALK_QUIESCENCE_POLL_MS);
        assert_eq!(walks(&env), 0, "no walk over a load still in flight");
        assert!(
            !LOAD_ARMED.load(Ordering::Relaxed),
            "re-armed for the next attempt"
        );
        assert_eq!(
            WALKS_THIS_LOAD.load(Ordering::Relaxed),
            0,
            "a gated attempt does not spend the walk budget"
        );
    }

    /// A reconnect or Retry during the wait zeroes the worker count and the
    /// failure flag the wait relied on: the walk must not run for the old
    /// attempt (freenet/river#763 review A1).
    #[test]
    fn a_new_load_attempt_during_the_wait_aborts_the_walk() {
        let _g = fresh_load();
        let env = ScriptedEnv::default();
        *env.workers.borrow_mut() = VecDeque::from([1]);
        env.new_attempt_during_wait.set(true);
        poke_and_drive(&env);
        assert_eq!(walks(&env), 0);
        assert!(!LOAD_ARMED.load(Ordering::Relaxed));
    }

    /// Gated attempts never burn the per-load walk budget: after any number
    /// of them, a load whose room set is complete still walks.
    #[test]
    fn gated_attempts_do_not_burn_the_walk_budget() {
        let _g = fresh_load();
        let env = ScriptedEnv::default();
        env.incomplete.set(true);
        for _ in 0..(MAX_WALKS_PER_LOAD * 2) {
            poke_and_drive(&env);
        }
        assert_eq!(walks(&env), 0);
        env.incomplete.set(false);
        poke_and_drive(&env);
        assert_eq!(walks(&env), 1);
    }

    /// A poke that lands while a walk runs (the sweep flagging an import) gets
    /// its own walk in the same session; a burst of pokes never exceeds the cap.
    #[test]
    fn a_poke_during_a_walk_is_walked_and_the_cap_holds() {
        let _g = fresh_load();
        let env = ScriptedEnv::default();
        *env.during_walk.borrow_mut() = Some(Box::new(poke_walk));
        poke_and_drive(&env);
        assert_eq!(walks(&env), 2, "the mid-walk poke must be served");

        for _ in 0..10 {
            poke_and_drive(&env);
        }
        assert_eq!(
            walks(&env) as u32,
            MAX_WALKS_PER_LOAD,
            "no more than the cap per page load"
        );
        assert!(!session().walk_in_flight);
    }

    // ---------------- the sweep gate's retries (review A2) ----------------

    fn scripted_gate(
        answers: Vec<SweepWrite>,
    ) -> (
        impl FnMut() -> std::future::Ready<SweepWrite>,
        std::rc::Rc<Cell<usize>>,
    ) {
        let calls = std::rc::Rc::new(Cell::new(0usize));
        let c = calls.clone();
        let answers = RefCell::new(VecDeque::from(answers));
        let gate = move || {
            c.set(c.get() + 1);
            std::future::ready(answers.borrow_mut().pop_front().unwrap_or(SweepWrite::Skip))
        };
        (gate, calls)
    }

    #[test]
    fn a_journal_refusal_is_retried_with_backoff_before_the_sweep_skips() {
        let slept = RefCell::new(Vec::new());
        let (gate, calls) = scripted_gate(vec![SweepWrite::Skip, SweepWrite::Merge]);
        let w = futures::executor::block_on(with_gate_retries(
            gate,
            |ms| {
                slept.borrow_mut().push(ms);
                std::future::ready(())
            },
            &SWEEP_GATE_BACKOFF_MS,
        ));
        assert_eq!(w, SweepWrite::Merge);
        assert_eq!(calls.get(), 2);
        assert_eq!(*slept.borrow(), vec![SWEEP_GATE_BACKOFF_MS[0]]);

        let slept = RefCell::new(Vec::new());
        let (gate, calls) = scripted_gate(vec![]);
        let w = futures::executor::block_on(with_gate_retries(
            gate,
            |ms| {
                slept.borrow_mut().push(ms);
                std::future::ready(())
            },
            &SWEEP_GATE_BACKOFF_MS,
        ));
        assert_eq!(w, SweepWrite::Skip, "bounded: it gives up");
        assert_eq!(calls.get(), 1 + SWEEP_GATE_BACKOFF_MS.len());
        assert_eq!(*slept.borrow(), SWEEP_GATE_BACKOFF_MS.to_vec());

        let (gate, calls) = scripted_gate(vec![SweepWrite::MergeUnflagged]);
        let w = futures::executor::block_on(with_gate_retries(
            gate,
            |_| std::future::ready(()),
            &SWEEP_GATE_BACKOFF_MS,
        ));
        assert_eq!(w, SweepWrite::MergeUnflagged);
        assert_eq!(calls.get(), 1, "an allowed merge is not retried");
    }

    /// Once a newer generation's data was skipped, the sweep holds back every
    /// older generation for the page load; the skipped one and newer ones are
    /// not held back by it.
    #[test]
    fn a_skipped_newer_generation_holds_back_older_ones() {
        assert!(!older_than_a_skipped_newer(5, 0), "nothing skipped");
        let skipped_28 = 28 + 1;
        assert!(older_than_a_skipped_newer(27, skipped_28));
        assert!(older_than_a_skipped_newer(0, skipped_28));
        assert!(
            !older_than_a_skipped_newer(28, skipped_28),
            "itself: retried on its own"
        );
        assert!(!older_than_a_skipped_newer(29, skipped_28), "newer");
    }
}
