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
    river_delegate_lineage, store_walk_cadence, top_up_in_window_with, walk_with, LiveImportSeam,
    NodeDelegateChannel, SweepWrite, TopUpOutcome, TopUpWindow,
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
    /// An earlier generation that may hold the user's rooms has not answered
    /// yet (pending, and not last found "not installed"): an identity imported
    /// now would take authoritative rank, and that generation's newer key would
    /// lose for good when it does answer (#768 round 2 item 10, probe R4).
    /// Waits like [`ImportGate::Pending`], but the user may override it,
    /// because a generation this node can never run may never answer.
    Unresolved,
}

/// Whether some pending predecessor may still bring data: it is unresolved
/// and its last walk did not find it definitively not installed (it was silent,
/// or it answered and its export was refused). A predecessor with an
/// `absent_streak` is absent from this node and holds nothing here.
pub(crate) fn a_pending_predecessor_may_hold_data(status: &JournalStatus) -> bool {
    match status {
        JournalStatus::Incomplete { pending, .. } => pending.iter().any(|p| p.absent_streak == 0),
        _ => false,
    }
}

/// Whether a journaled walk running now may still bring rooms back: it is
/// requested or running, and the journal says a pending predecessor may hold
/// data (it has not been found "not installed") or an import was interrupted.
/// Drives the rail's "Restoring more rooms…" note while a partly restored list
/// is already showing (freenet/river#802). A walk whose pending predecessors
/// were all found "not installed" (the usual case once a migration is done)
/// shows nothing. A predecessor not yet walked, or one that stays silent (it
/// cannot run here, or the node predates typed absence) or was refused for a
/// short index, may still hold rooms, so a walk over it shows the note for the
/// walk's duration; after `SILENT_MIN_ATTEMPTS` walks those walk only on one
/// load in `SILENT_WALK_EVERY_N_LOADS`.
pub(crate) fn walk_may_restore_rooms(s: &JournalSession) -> bool {
    s.walk_in_flight
        && s.status
            .as_ref()
            .is_some_and(|st| a_pending_predecessor_may_hold_data(st) || st.interrupted_import())
}

/// Test hook (example-data + no-sync builds only): put the session in the
/// state of a walk over predecessors that may still hold rooms (`true`), or
/// end that walk (`false`), so a browser test can drive the rail's note
/// through the same version counter a real walk bumps (freenet/river#802).
#[cfg(all(target_arch = "wasm32", feature = "example-data", feature = "no-sync"))]
pub(crate) fn test_set_walk_restoring(on: bool) {
    let status = freenet_migrate::MigrationJournal::new_pending(&LINEAGE).status(
        &LINEAGE,
        &freenet_migrate::JournalBinding::new(&super::delegate_migration::river_secret_policy(), 0),
    );
    update_session(move |s| {
        s.walk_in_flight = on;
        if on {
            s.status = Some(status);
        }
    });
}

/// [`walk_may_restore_rooms`] as it stands now. Callers that render it read
/// [`JOURNAL_GATE_VERSION`] first, so they re-render when it changes.
pub(crate) fn current_walk_may_restore_rooms() -> bool {
    walk_may_restore_rooms(&session())
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
/// * a pending predecessor that has not answered (or whose export was refused)
///   and was not last found "not installed" → [`ImportGate::Unresolved`]:
///   waits, overridable (#768 round 2 item 10).
///
/// A predecessor found definitively not installed (its `absent_streak`) does
/// NOT gate: it keeps the journal incomplete for as long as its generation is
/// absent from this node (most generations, on most nodes), so gating on
/// `!is_complete()` would block for good.
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
    if override_on {
        return ImportGate::Open;
    }
    if blocked {
        ImportGate::Blocked
    } else if s
        .status
        .as_ref()
        .is_some_and(a_pending_predecessor_may_hold_data)
    {
        ImportGate::Unresolved
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
/// A pending predecessor that never answers typed-absent (it is installed but
/// cannot execute, or the node predates typed absence: freenet-core before
/// 0.2.137 answers a missing delegate with an empty success, and every core
/// answers a delegate that fails to execute with an untyped error) counts as
/// persistently silent after this many walks left it unresolved.
pub(crate) const SILENT_MIN_ATTEMPTS: u32 = 6;
/// While every pending predecessor is consistently absent or persistently
/// silent, and at least one is silent, walk on one load in this many. Shorter
/// than [`WALK_EVERY_N_LOADS`]: a silent predecessor may still hold data and
/// answer later, so it is retried more often than one the node says it does not
/// have.
pub(crate) const SILENT_WALK_EVERY_N_LOADS: u32 = 4;

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

/// The walk cadence for `status`: `None` when every load must walk, `Some(n)`
/// to walk on one load in `n` (freenet/river#761).
///
/// A rate limit applies only while nothing is mid-import and every pending
/// predecessor is either consistently absent ([`CADENCE_ABSENT_STREAK`] typed
/// "not installed" walks in a row) or persistently silent (left unresolved by
/// [`SILENT_MIN_ATTEMPTS`] walks). All absent: [`WALK_EVERY_N_LOADS`]. Any
/// silent: [`SILENT_WALK_EVERY_N_LOADS`]. Neither is a seal: the walk keeps
/// running on the cadence and resolves whatever answers.
///
/// `short_refused` lists the predecessors refused for a short index whose
/// provable rooms were recovered (`SHORT_INDEX_KEY`). Such a predecessor stays
/// mid-import for good once the sweep flagged it, yet nothing a walk does can
/// change its answer while its index is unchanged, so it counts toward the
/// silent cadence like a refusal (#768 round 4, S-C). Any other mid-import
/// predecessor walks on every load.
pub(crate) fn walk_cadence(status: &JournalStatus, short_refused: &[[u8; 32]]) -> Option<u32> {
    if status.is_complete() {
        return None;
    }
    let JournalStatus::Incomplete { pending, .. } = status else {
        return None;
    };
    let started =
        |p: &freenet_migrate::PendingPredecessor| p.import == freenet_migrate::ImportFlag::Started;
    let refused = |p: &freenet_migrate::PendingPredecessor| {
        short_refused.iter().any(|k| k[..] == p.key.bytes()[..])
    };
    if pending.iter().any(|p| started(p) && !refused(p)) {
        return None;
    }
    if status.pending_all_absent(CADENCE_ABSENT_STREAK) {
        return Some(WALK_EVERY_N_LOADS);
    }
    let settled = !pending.is_empty()
        && pending
            .iter()
            .all(|p| p.absent_streak >= CADENCE_ABSENT_STREAK || p.attempts >= SILENT_MIN_ATTEMPTS);
    settled.then_some(SILENT_WALK_EVERY_N_LOADS)
}

/// Whether the cadence counter must be read to decide the walk: only for a
/// status the rate limit can apply to (see [`walk_decision`]).
pub(crate) fn needs_cadence_counter(status: &JournalStatus, short_refused: &[[u8; 32]]) -> bool {
    walk_cadence(status, short_refused).is_some()
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
/// pending, so the walk would otherwise re-probe every generation on every
/// load for good. [`walk_cadence`] is a RATE LIMIT only: the walk runs on one
/// load in the cadence it returns, counted in River's own KV (`skips`: `None`
/// when the counter could not be read, which runs the walk). An interrupted
/// import, or a predecessor that is neither consistently absent nor
/// persistently silent yet, always walks.
pub(crate) fn walk_decision(
    status: &JournalStatus,
    short_refused: &[[u8; 32]],
    skips: Option<u32>,
) -> WalkDecision {
    if status.is_complete() {
        return WalkDecision::NotNeeded;
    }
    let Some(every) = walk_cadence(status, short_refused) else {
        return WalkDecision::Run;
    };
    match skips {
        None => WalkDecision::Run,
        Some(n) if n.saturating_add(1) >= every => WalkDecision::RunResetCadence,
        Some(n) => WalkDecision::Skip { skips: n + 1 },
    }
}

/// What a page load does once `begin` answered and the walk is decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoadPlan {
    /// Ask the driver for a journaled walk.
    pub(crate) walk: bool,
    /// Run the post-resolve top-up on this load (with the walk when there is
    /// one, else on its own: it never forces a walk).
    pub(crate) topup: bool,
    /// Restart the cadence count, but only once the walk actually runs
    /// (#768 round 3 CF6): a walk the room-set gate refuses must not spend it.
    pub(crate) reset_cadence_on_walk: bool,
}

/// The post-`begin` decision of [`start_journal_load`], as a pure function so
/// the #761 wiring is pinned: a skipped or unneeded walk never pokes the
/// driver for a walk, and the top-up follows only the newest predecessor's
/// resolution.
pub(crate) fn load_plan(decision: WalkDecision, newest_resolved: bool) -> LoadPlan {
    let walk = matches!(decision, WalkDecision::Run | WalkDecision::RunResetCadence);
    LoadPlan {
        walk,
        topup: newest_resolved,
        reset_cadence_on_walk: decision == WalkDecision::RunResetCadence,
    }
}

// ---------------------------------------------------------------------------
// The post-resolve top-up window (freenet/river#768 review B1)
// ---------------------------------------------------------------------------

/// How long, and for how many top-ups, the newest predecessor is re-read
/// after the journal resolves it. An old tab still running the previous build
/// keeps writing to it until it is closed; the merging covers a rollout. A
/// change to the predecessor's key index restarts the window (#768 round 2
/// item 8), so the window ends only after the old tab's last key change, not
/// after a fixed count of page loads. Deciding the window still costs two
/// small reads on every load after it ends (the predecessor's raw index, to
/// see whether it changed, and the stored window), for as long as the newest
/// predecessor is the one this build resolved.
pub(crate) const TOPUP_WINDOW_MS: u64 = 14 * 24 * 60 * 60 * 1000;
pub(crate) const TOPUP_WINDOW_LOADS: u32 = 30;

/// What this load does about the top-up window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TopUp {
    /// Top up on this load from `window` (restarted if the index changed);
    /// the caller counts the run only once it ran.
    Run { window: TopUpWindow },
    /// The window is over (by time or by runs) and the index is unchanged.
    Ended,
}

/// Decide the top-up from the persisted window (`None`: not started), the
/// time, and the digest of the predecessor's key index just read. Pure.
pub(crate) fn topup_decision(state: Option<TopUpWindow>, now_ms: u64, digest: [u8; 32]) -> TopUp {
    let restart = TopUpWindow {
        started_ms: now_ms,
        runs: 0,
        index_digest: digest,
    };
    match state {
        None => TopUp::Run { window: restart },
        Some(w) if w.index_digest != digest => TopUp::Run { window: restart },
        Some(w)
            if w.runs >= TOPUP_WINDOW_LOADS
                || now_ms.saturating_sub(w.started_ms) >= TOPUP_WINDOW_MS =>
        {
            TopUp::Ended
        }
        Some(w) => TopUp::Run { window: w },
    }
}

/// Whether the journal resolves `newest` (the newest generation in the
/// lineage) in `status`.
pub(crate) fn newest_resolved(status: &JournalStatus, newest: &DelegateLineageEntry) -> bool {
    match status {
        JournalStatus::Complete { .. } => true,
        JournalStatus::Incomplete { pending, .. } => pending
            .iter()
            .all(|p| p.key.bytes() != newest.delegate_key.as_slice()),
        _ => false,
    }
}

fn now_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as u64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A top-up is due on this load's next driver run.
static TOPUP_PENDING: AtomicBool = AtomicBool::new(false);

/// A walk is due on this load's next driver run (as opposed to a top-up only).
static WALK_WANTED: AtomicBool = AtomicBool::new(false);

/// The cadence count restarts when this load's walk actually runs.
static RESET_CADENCE_ON_WALK: AtomicBool = AtomicBool::new(false);

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
            // The predecessors refused for a short index (S-C). Unreadable:
            // none, which only walks more often.
            let short_refused: Vec<[u8; 32]> = if status.is_complete() {
                Vec::new()
            } else {
                super::delegate_migration::read_short_index_record(&channel, &successor)
                    .await
                    .map(|r| r.into_iter().map(|(k, _)| k).collect())
                    .unwrap_or_default()
            };
            let skips = if needs_cadence_counter(&status, &short_refused) {
                read_walk_cadence(&channel, &successor).await.ok()
            } else {
                None
            };
            let decision = walk_decision(&status, &short_refused, skips);
            match decision {
                WalkDecision::Skip { skips } => {
                    match store_walk_cadence(&channel, &successor, skips).await {
                        Ok(()) => decision,
                        // A skip that is not counted could repeat for good.
                        Err(_) => WalkDecision::Run,
                    }
                }
                _ => decision,
            }
        };
        info!("migration journal: walk decision for this load: {decision:?}");
        // The post-resolve top-up (#768 review B1) once the newest
        // predecessor is resolved. It runs on its own: it never forces the
        // walk (which the #761 cadence may be skipping this load).
        let plan = load_plan(
            decision,
            LINEAGE
                .iter()
                .max_by_key(|e| e.generation)
                .is_some_and(|newest| newest_resolved(&status, newest)),
        );
        if plan.walk {
            if plan.topup {
                TOPUP_PENDING.store(true, Ordering::Relaxed);
            }
            RESET_CADENCE_ON_WALK.store(plan.reset_cadence_on_walk, Ordering::Relaxed);
            poke_walk();
        } else if plan.topup {
            poke_topup();
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

/// The sweep's journal gate for data read from the legacy delegate
/// `responding` (freenet/river#763, #768 review A2): [`gate_sweep_import`],
/// retried with bounded backoff. The caller marks the load's fetch failure on
/// a final skip, so the UI offers Retry rather than a false "no rooms"; that
/// mark does NOT gate the walk (the walk's gate reads only the CURRENT
/// delegate's load, `chat_delegate::room_set_complete`), so the walk still
/// imports what the sweep could not merge. There is no hold-back of older
/// generations: B3's persisted identity sources already keep a newer
/// generation's identity winning across loads, and a hold-back only blocked
/// the walk it meant to hand them to.
pub(crate) async fn gate_sweep_import_retrying(
    responding: &DelegateKey,
    items: Option<u32>,
) -> SweepWrite {
    with_gate_retries(
        || gate_sweep_import(responding, items),
        |ms| crate::util::sleep(std::time::Duration::from_millis(ms)),
        &SWEEP_GATE_BACKOFF_MS,
    )
    .await
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
    WALK_WANTED.store(true, Ordering::Relaxed);
    update_session(|s| s.walk_in_flight = true);
    poke_driver();
}

/// Ask for the post-resolve top-up alone (no walk, so the import gate is not
/// held Pending for it).
pub(crate) fn poke_topup() {
    TOPUP_PENDING.store(true, Ordering::Relaxed);
    poke_driver();
}

fn poke_driver() {
    WALK_POKED.store(true, Ordering::Relaxed);
    if WALK_DRIVER_RUNNING.swap(true, Ordering::Relaxed) {
        return;
    }
    crate::util::safe_spawn_local(walk_driver());
}

/// What one driver run does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DriverWork {
    /// A journaled walk (counted against the per-load budget).
    pub(crate) walk: bool,
    /// The post-resolve top-up (not counted).
    pub(crate) topup: bool,
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
    /// Run `work` to completion on behalf of load attempt `attempt` (its
    /// seams re-check the room set before every merge and their flush).
    fn run_work(&self, attempt: u32, work: DriverWork) -> impl Future<Output = ()>;
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
    async fn run_work(&self, attempt: u32, work: DriverWork) {
        run_work_locked(attempt, work).await
    }
}

async fn walk_driver() {
    walk_driver_with(&LiveWalkEnv).await
}

async fn walk_driver_with<E: WalkEnv>(env: &E) {
    loop {
        while WALK_POKED.swap(false, Ordering::Relaxed) {
            if WALK_WANTED.load(Ordering::Relaxed)
                && WALKS_THIS_LOAD.load(Ordering::Relaxed) >= MAX_WALKS_PER_LOAD
            {
                warn!(
                    "migration journal: {MAX_WALKS_PER_LOAD} walks this page load; the next \
                     load continues"
                );
                WALK_WANTED.store(false, Ordering::Relaxed);
            }
            if !WALK_WANTED.load(Ordering::Relaxed) && !TOPUP_PENDING.load(Ordering::Relaxed) {
                continue;
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
                // Let the next load attempt (a reconnect, or Retry) start again;
                // it decides the walk and the top-up afresh. A gated attempt
                // spends neither the walk budget nor a top-up run.
                WALK_WANTED.store(false, Ordering::Relaxed);
                TOPUP_PENDING.store(false, Ordering::Relaxed);
                LOAD_ARMED.store(false, Ordering::Relaxed);
                continue;
            }
            let work = DriverWork {
                walk: WALK_WANTED.swap(false, Ordering::Relaxed),
                topup: TOPUP_PENDING.swap(false, Ordering::Relaxed),
            };
            if work.walk {
                WALKS_THIS_LOAD.fetch_add(1, Ordering::Relaxed);
            }
            env.run_work(attempt, work).await;
        }
        WALK_DRIVER_RUNNING.store(false, Ordering::Relaxed);
        // A poke that landed after the last check found the driver still
        // running and spawned nothing: pick it up here.
        let resume = WALK_POKED.load(Ordering::Relaxed)
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

/// One driver run under the journal lock, on behalf of load attempt
/// `attempt`: the post-resolve top-up and/or a journaled walk (recording its
/// status). The top-up shares the walk's gate (a complete room set for this
/// attempt) and its seam's re-checks.
async fn run_work_locked(attempt: u32, work: DriverWork) {
    let _ops = JOURNAL_OPS.lock().await;
    if work.topup {
        if let Some(newest) = LINEAGE.iter().max_by_key(|e| e.generation) {
            match top_up_in_window_with(
                &NodeDelegateChannel,
                &mut LiveImportSeam::new(attempt),
                &chat_delegate::current_delegate_key(),
                newest,
                now_ms(),
            )
            .await
            {
                Ok(TopUpOutcome::Ran { merged, window }) => info!(
                    "migration journal: top-up merged {merged} item(s) from the newest \
                     predecessor (run {} of the window)",
                    window.runs
                ),
                Ok(TopUpOutcome::Ended) => {}
                Err(e) => {
                    warn!("migration journal: top-up did not run ({e}); the next load retries")
                }
            }
        }
    }
    if !work.walk {
        return;
    }
    if RESET_CADENCE_ON_WALK.swap(false, Ordering::Relaxed) {
        let _ = store_walk_cadence(
            &NodeDelegateChannel,
            &chat_delegate::current_delegate_key(),
            0,
        )
        .await;
    }
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

    /// freenet/river#802: the rail's "Restoring more rooms…" note follows a
    /// walk that may still bring rooms, and nothing else.
    #[test]
    fn walk_may_restore_rooms_only_while_a_walk_may_bring_data() {
        let mut s = known(fresh_status());
        assert!(
            !walk_may_restore_rooms(&s),
            "no walk in flight: nothing is being restored"
        );
        s.walk_in_flight = true;
        assert!(
            walk_may_restore_rooms(&s),
            "a walk over predecessors that may hold data"
        );
        let mut done = known(JournalStatus::Complete { rejected_items: 0 });
        done.walk_in_flight = true;
        assert!(
            !walk_may_restore_rooms(&done),
            "a complete journal brings nothing"
        );
        let unknown = JournalSession {
            walk_in_flight: true,
            ..Default::default()
        };
        assert!(!walk_may_restore_rooms(&unknown), "no status read yet");
        // The steady state once a migration is done on this node: every
        // pending predecessor is typed "not installed". No note, or every
        // user's rail would carry one on every walk.
        let mut absent = known(all_absent_status());
        absent.walk_in_flight = true;
        assert!(
            !walk_may_restore_rooms(&absent),
            "absent predecessors bring nothing"
        );
        // An interrupted import is being finished: rooms may still arrive.
        let mut interrupted = known(interrupted_status());
        interrupted.walk_in_flight = true;
        assert!(walk_may_restore_rooms(&interrupted));
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

    /// A journal kept incomplete only by predecessors found NOT INSTALLED
    /// (absent generations) must not gate: that is most nodes, for good.
    #[test]
    fn an_incomplete_journal_of_absent_generations_does_not_gate() {
        let status = all_absent_status();
        assert!(!status.is_complete());
        assert!(!status.interrupted_import());
        assert_eq!(import_gate(&known(status), false), ImportGate::Open);
    }

    /// #768 round 2 item 10 (migration probe R4): while a predecessor that may
    /// hold the user's rooms has not answered (the newest one silent, the
    /// current delegate empty), an imported identity would take authoritative
    /// rank and that generation's newer key would lose for good when it
    /// answers. The gate waits (Unresolved), the user may override it, and a
    /// generation found not installed stops gating.
    #[test]
    fn an_unanswered_predecessor_gates_the_import_until_overridden() {
        let status = fresh_status();
        assert!(a_pending_predecessor_may_hold_data(&status));
        assert_eq!(
            import_gate(&known(status.clone()), false),
            ImportGate::Unresolved
        );
        assert_eq!(import_gate(&known(status), true), ImportGate::Open);
        assert!(
            crate::components::app::chat_delegate::recovery_in_progress(ImportGate::Unresolved, 0),
            "the import waits while unresolved"
        );
        // Only one of several pending predecessors unanswered still gates.
        let mut mixed = all_absent_status();
        if let JournalStatus::Incomplete { pending, .. } = &mut mixed {
            pending.last_mut().expect("pending").absent_streak = 0;
        }
        assert_eq!(import_gate(&known(mixed), false), ImportGate::Unresolved);
        // A blocking input still reads Blocked (its message is the stronger).
        let mut s = known(fresh_status());
        s.walk_status_unknown = true;
        assert_eq!(import_gate(&s, false), ImportGate::Blocked);
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
            walk_decision(&JournalStatus::Complete { rejected_items: 0 }, &[], None),
            WalkDecision::NotNeeded
        );
        // Nothing typed-absent yet: always walk, never read the counter.
        assert_eq!(
            walk_decision(&fresh_status(), &[], Some(3)),
            WalkDecision::Run
        );
        assert_eq!(
            walk_decision(&interrupted_status(), &[], Some(3)),
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
        assert_eq!(walk_decision(&status, &[], None), WalkDecision::Run);
        let mut skips = 0;
        let mut walked_at = None;
        for load in 1..=WALK_EVERY_N_LOADS {
            match walk_decision(&status, &[], Some(skips)) {
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

    /// freenet/river#768 round 3 (CF2, S5): a predecessor that is installed but
    /// never answers (it cannot execute, or the node predates typed absence)
    /// keeps the journal incomplete for good. It must not walk on every load:
    /// after `SILENT_MIN_ATTEMPTS` walks it is rate-limited, more often than a
    /// typed-absent one, and never before.
    #[test]
    fn a_persistently_silent_predecessor_is_rate_limited_but_retried() {
        use super::super::delegate_migration::tests_support::silent_status;
        let early = silent_status(SILENT_MIN_ATTEMPTS - 1);
        assert_eq!(
            walk_cadence(&early, &[]),
            None,
            "not before SILENT_MIN_ATTEMPTS walks"
        );
        assert_eq!(walk_decision(&early, &[], Some(0)), WalkDecision::Run);
        let settled = silent_status(SILENT_MIN_ATTEMPTS);
        assert_eq!(walk_cadence(&settled, &[]), Some(SILENT_WALK_EVERY_N_LOADS));
        assert!(needs_cadence_counter(&settled, &[]));
        assert_eq!(
            walk_decision(&settled, &[], Some(0)),
            WalkDecision::Skip { skips: 1 }
        );
        assert_eq!(
            walk_decision(&settled, &[], Some(SILENT_WALK_EVERY_N_LOADS - 1)),
            WalkDecision::RunResetCadence
        );
        assert_eq!(
            walk_decision(&settled, &[], None),
            WalkDecision::Run,
            "an unreadable counter walks"
        );
        assert!(SILENT_WALK_EVERY_N_LOADS < WALK_EVERY_N_LOADS);
        assert_eq!(
            walk_cadence(&all_absent_status(), &[]),
            Some(WALK_EVERY_N_LOADS)
        );
        assert_eq!(
            walk_cadence(&interrupted_status(), &[]),
            None,
            "an interrupted import always walks"
        );
        assert_eq!(walk_cadence(&fresh_status(), &[]), None);
    }

    /// freenet/river#761 at the wiring (#768 round 3 CF1): only a Run or
    /// RunResetCadence decision pokes a walk; Skip and NotNeeded never do; the
    /// top-up follows the newest predecessor's resolution alone; and only
    /// RunResetCadence restarts the count, and only once the walk runs (CF6).
    #[test]
    fn the_load_plan_walks_only_when_the_decision_says_so() {
        for topup in [false, true] {
            assert_eq!(
                load_plan(WalkDecision::Run, topup),
                LoadPlan {
                    walk: true,
                    topup,
                    reset_cadence_on_walk: false
                }
            );
            assert_eq!(
                load_plan(WalkDecision::RunResetCadence, topup),
                LoadPlan {
                    walk: true,
                    topup,
                    reset_cadence_on_walk: true
                }
            );
            assert_eq!(
                load_plan(WalkDecision::Skip { skips: 3 }, topup),
                LoadPlan {
                    walk: false,
                    topup,
                    reset_cadence_on_walk: false
                }
            );
            assert_eq!(
                load_plan(WalkDecision::NotNeeded, topup),
                LoadPlan {
                    walk: false,
                    topup,
                    reset_cadence_on_walk: false
                }
            );
        }
    }

    /// The wasm-only `start_journal_load` and driver act on the plan and
    /// nothing else (source pin: they run only in the browser). The walk is
    /// poked only under `plan.walk`; the cadence reset is no longer written at
    /// decision time; and it is written in `run_work_locked` after the walk
    /// gate let the walk through, before the walk itself.
    #[test]
    fn start_journal_load_acts_only_on_the_load_plan() {
        // Brace-scoped (#768 round 4): each call must sit INSIDE the block of
        // the condition that guards it, not merely after it in the text.
        // Comments are stripped and string literals skipped when matching
        // braces, so neither can satisfy or confuse the pin.
        let src = crate::util::strip_comments(include_str!("migration_journal.rs"));
        let src = src.as_str();
        let start = src
            .find("pub(crate) fn start_journal_load()")
            .expect("start_journal_load");
        let body = block(src, start);
        assert!(
            body.contains("let plan = load_plan("),
            "the decision goes through load_plan"
        );
        let walk_block = block(body, body.find("if plan.walk {").expect("plan.walk"));
        assert!(walk_block.contains("poke_walk();"));
        assert!(walk_block.contains("RESET_CADENCE_ON_WALK.store(plan.reset_cadence_on_walk"));
        assert_eq!(
            body.matches("poke_walk();").count(),
            1,
            "poke_walk only under plan.walk"
        );
        let topup_block = block(
            body,
            body.find("} else if plan.topup {").expect("plan.topup") + 2,
        );
        assert!(topup_block.contains("poke_topup();"));
        assert_eq!(body.matches("poke_topup();").count(), 1);
        assert!(
            !body.contains("store_walk_cadence(&channel, &successor, 0)"),
            "the cadence reset must not be written before the walk runs"
        );
        // S-C (#768 round 4): the recorded short-index refusals reach the
        // cadence. They are read from the record, and both the counter gate and
        // the decision receive them (not an empty list).
        assert!(body.contains("read_short_index_record(&channel, &successor)"));
        let squeezed: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(squeezed.contains("needs_cadence_counter(&status,&short_refused)"));
        assert!(squeezed.contains("walk_decision(&status,&short_refused,skips)"));
        // In the driver: the reset happens at the function's own level, after
        // the early return of a run without a walk, and before the walk.
        let run = block(
            src,
            src.find("async fn run_work_locked(")
                .expect("run_work_locked"),
        );
        let gate_at = run.find("if !work.walk {").expect("walk gate");
        let gate = block(run, gate_at);
        assert!(gate.contains("return;"));
        let reset = run
            .find("RESET_CADENCE_ON_WALK.swap(false")
            .expect("reset at walk time");
        let walk = run.find("walk_with(").expect("walk");
        assert!(gate_at + gate.len() < reset && reset < walk);
        assert_eq!(
            depth_at(run, reset),
            1,
            "the reset is not inside another block"
        );
    }

    /// The `{ ... }` block opened by the first `{` at or after `from`, string
    /// literals skipped.
    fn block(src: &str, from: usize) -> &str {
        let open = from + src[from..].find('{').expect("a block");
        let mut depth = 0i32;
        let mut in_str = false;
        let mut escaped = false;
        for (i, c) in src[open..].char_indices() {
            if in_str {
                match (escaped, c) {
                    (true, _) => escaped = false,
                    (false, '\\') => escaped = true,
                    (false, '"') => in_str = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &src[open..open + i + 1];
                    }
                }
                _ => {}
            }
        }
        panic!("unterminated block");
    }

    /// Brace depth at `pos` inside `block_src` (1 = the block's own level).
    fn depth_at(block_src: &str, pos: usize) -> i32 {
        let mut depth = 0i32;
        let mut in_str = false;
        let mut escaped = false;
        for c in block_src[..pos].chars() {
            if in_str {
                match (escaped, c) {
                    (true, _) => escaped = false,
                    (false, '\\') => escaped = true,
                    (false, '"') => in_str = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        depth
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
        assert!(needs_cadence_counter(&all_absent_status(), &[]));
        assert!(!needs_cadence_counter(
            &JournalStatus::Complete { rejected_items: 0 },
            &[]
        ));
        assert!(
            !needs_cadence_counter(&fresh_status(), &[]),
            "nothing typed-absent yet"
        );
        assert!(!needs_cadence_counter(&interrupted_status(), &[]));
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
        WALK_WANTED.store(false, Ordering::Relaxed);
        TOPUP_PENDING.store(false, Ordering::Relaxed);
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
        async fn run_work(&self, _attempt: u32, work: DriverWork) {
            if work.topup {
                self.log.borrow_mut().push("topup");
            }
            if work.walk {
                self.log.borrow_mut().push("walk");
            }
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

    fn topups(env: &ScriptedEnv) -> usize {
        env.log.borrow().iter().filter(|e| **e == "topup").count()
    }

    /// #768 round 2 (Consider): the post-resolve top-up runs on its own. It
    /// never forces a walk (the #761 cadence may be skipping this load), never
    /// spends the walk budget, and never holds the import gate Pending.
    #[test]
    fn a_topup_alone_runs_without_a_walk() {
        let _g = fresh_load();
        let env = ScriptedEnv::default();
        poke_topup();
        assert!(!session().walk_in_flight, "a top-up does not hold the gate");
        futures::executor::block_on(walk_driver_with(&env));
        assert_eq!((topups(&env), walks(&env)), (1, 0));
        assert_eq!(WALKS_THIS_LOAD.load(Ordering::Relaxed), 0);
        // Even with the walk budget spent, a top-up still runs.
        WALKS_THIS_LOAD.store(MAX_WALKS_PER_LOAD, Ordering::Relaxed);
        poke_topup();
        futures::executor::block_on(walk_driver_with(&env));
        assert_eq!(topups(&env), 2);
    }

    /// A walk and a top-up requested together run once each, together.
    #[test]
    fn a_walk_and_a_topup_run_together() {
        let _g = fresh_load();
        let env = ScriptedEnv::default();
        poke_topup();
        poke_and_drive(&env);
        assert_eq!(*env.log.borrow(), vec!["topup", "walk"]);
    }

    /// A gated attempt runs no top-up and clears the request, so the next
    /// load attempt decides it afresh (and nothing is counted).
    #[test]
    fn a_gated_attempt_runs_no_topup() {
        let _g = fresh_load();
        let env = ScriptedEnv::default();
        env.incomplete.set(true);
        poke_topup();
        futures::executor::block_on(walk_driver_with(&env));
        assert_eq!(topups(&env), 0);
        assert!(!TOPUP_PENDING.load(Ordering::Relaxed));
        assert!(!LOAD_ARMED.load(Ordering::Relaxed));
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

    // ---------------- the post-resolve top-up window (review B1) ----------------

    #[test]
    fn the_topup_window_runs_then_ends_by_runs_or_time_and_restarts_on_change() {
        let t0 = 1_000_000;
        let d = [1u8; 32];
        let fresh = |t| TopUpWindow {
            started_ms: t,
            runs: 0,
            index_digest: d,
        };
        assert_eq!(
            topup_decision(None, t0, d),
            TopUp::Run { window: fresh(t0) }
        );
        // Count only what ran (the caller adds the run): bounded by runs.
        let mut w = fresh(t0);
        let mut runs = 0;
        while let TopUp::Run { window } = topup_decision(Some(w), t0 + 1, d) {
            w = TopUpWindow {
                runs: window.runs + 1,
                ..window
            };
            runs += 1;
            assert!(runs <= TOPUP_WINDOW_LOADS, "bounded by runs");
        }
        assert_eq!(runs, TOPUP_WINDOW_LOADS);
        // A load that did not run records nothing, so it cannot end the window.
        let one = TopUpWindow {
            runs: 1,
            ..fresh(t0)
        };
        assert_eq!(
            topup_decision(Some(one), t0 + 1, d),
            TopUp::Run { window: one }
        );
        assert_eq!(
            topup_decision(Some(one), t0 + TOPUP_WINDOW_MS, d),
            TopUp::Ended,
            "bounded by time"
        );
        assert_eq!(
            topup_decision(Some(one), t0.saturating_sub(5), d),
            TopUp::Run { window: one },
            "a clock that went back does not end or extend it"
        );
        // The predecessor's index changed (an old tab wrote a new key): the
        // window restarts, even after it had ended.
        let changed = [2u8; 32];
        let t1 = t0 + TOPUP_WINDOW_MS * 3;
        assert_eq!(
            topup_decision(Some(w), t1, changed),
            TopUp::Run {
                window: TopUpWindow {
                    started_ms: t1,
                    runs: 0,
                    index_digest: changed
                }
            }
        );
    }

    #[test]
    fn the_newest_generation_is_resolved_only_when_not_pending() {
        let lineage = river_delegate_lineage(&[([1u8; 32], [2u8; 32]), ([3u8; 32], [4u8; 32])]);
        let newest = &lineage[1];
        assert!(!newest_resolved(&fresh_status(), newest), "both pending");
        assert!(newest_resolved(
            &JournalStatus::Complete { rejected_items: 0 },
            newest
        ));
    }

    /// The "Import anyway" branch (#768 review A6): a journal that blocks the
    /// identity import is overridden by the user's choice for this page load,
    /// without resolving anything (the session still reports the interrupted
    /// import), and the override never skips a walk that is about to answer.
    #[test]
    fn import_anyway_opens_a_blocked_gate_without_resolving_it() {
        let _g = fresh_load();
        /// Clears the process-global override even when an assertion fails.
        struct ClearOverride;
        impl Drop for ClearOverride {
            fn drop(&mut self) {
                GATE_OVERRIDE.store(false, Ordering::Relaxed);
            }
        }
        let _clear = ClearOverride;
        GATE_OVERRIDE.store(false, Ordering::Relaxed);
        *session() = known(interrupted_status());
        assert_eq!(current_import_gate(), ImportGate::Blocked);
        assert!(
            crate::components::app::chat_delegate::recovery_in_progress(current_import_gate(), 0),
            "the import dialog waits"
        );

        override_import_gate();
        assert_eq!(current_import_gate(), ImportGate::Open);
        assert!(
            !crate::components::app::chat_delegate::recovery_in_progress(current_import_gate(), 0)
        );
        assert!(
            session().status.as_ref().unwrap().interrupted_import(),
            "the override resolves nothing"
        );

        session().walk_in_flight = true;
        assert_eq!(current_import_gate(), ImportGate::Pending);
    }
}
