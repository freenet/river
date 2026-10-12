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
            let needs_counter = !status.is_complete()
                && !status.interrupted_import()
                && status.pending_all_absent(CADENCE_ABSENT_STREAK);
            let skips = if needs_counter {
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
            update_session(move |s| {
                s.begun = true;
                s.begin_failed = false;
                s.status = Some(recorded);
                s.sweep_flagged_since_status = false;
            });
            Some(status)
        }
        Err(e) => {
            warn!("migration journal: begin failed ({e}); no migrated write will happen until it succeeds");
            update_session(|s| s.begin_failed = !s.begun);
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
        {
            let s = session();
            if let Some(status) = &s.status {
                return status.interrupted_import() || s.sweep_flagged_since_status;
            }
            if s.begin_failed {
                return true;
            }
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
    if write == SweepWrite::Merge {
        update_session(|s| s.sweep_flagged_since_status = true);
        poke_walk();
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

async fn walk_driver() {
    loop {
        while WALK_POKED.swap(false, Ordering::Relaxed) {
            if WALKS_THIS_LOAD.fetch_add(1, Ordering::Relaxed) >= MAX_WALKS_PER_LOAD {
                warn!(
                    "migration journal: {MAX_WALKS_PER_LOAD} walks this page load; the next \
                     load continues"
                );
                break;
            }
            let mut waited = 0u64;
            while chat_delegate::pending_load_workers() != 0 && waited < WALK_QUIESCENCE_MAX_MS {
                crate::util::sleep(std::time::Duration::from_millis(WALK_QUIESCENCE_POLL_MS)).await;
                waited += WALK_QUIESCENCE_POLL_MS;
            }
            run_walk_locked().await;
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

/// One journaled walk under the journal lock, recording its status.
async fn run_walk_locked() {
    let _ops = JOURNAL_OPS.lock().await;
    let (report, _seam) = walk_with(
        &NodeDelegateChannel,
        LiveImportSeam::new(),
        chat_delegate::current_delegate_key(),
        &LINEAGE,
    )
    .await;
    log_migration_report(&report);
    let status = report.status.clone();
    update_session(move |s| {
        s.walk_status_unknown = status.is_none();
        if let Some(status) = status {
            s.begun = true;
            s.begin_failed = false;
            s.status = Some(status);
            // Journal operations are serialised, so every sweep flag raised
            // before this walk took the lock is in the status it returned.
            s.sweep_flagged_since_status = false;
        }
    });
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
}
