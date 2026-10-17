//! Delegate secret migration on `freenet-migrate` 0.7 (freenet/river#398 phase 3),
//! with the crate's durable completeness journal (freenet/river#763).
//!
//! A delegate's address is `BLAKE3(BLAKE3(wasm) ‖ params)`, so any rebuild
//! re-keys it and the successor starts with an EMPTY secret store. River's
//! delegate holds each room's identity key (`RoomData::self_sk`), which cannot
//! be re-derived from the network — losing it loses the user's identity in that
//! room. River has hand-rolled the recovery sweep in `chat_delegate.rs` /
//! `response_handler.rs` for 29 generations; this module runs the shared,
//! reviewed library's walk ALONGSIDE that sweep, so the walk order, marker
//! bookkeeping, withholding and per-predecessor classification come from one
//! place, and the crate's completeness journal is the only authority on
//! whether a migration finished. Retiring the hand-rolled sweep is a later
//! step, after field validation — nothing here removes it.
//!
//! # A UI-side adapter over River's existing delegate protocol
//!
//! The adapter needs nothing new from the delegate: River's delegate protocol
//! already has both halves of the writer seam. `GetRequest`/`ListRequest` read a
//! predecessor, and the current delegate's Store/CAS paths (via
//! `save_rooms_to_delegate`'s per-room CAS `reconcile_room_present`) are the
//! app's own import path. The journal and the markers are ordinary KV entries in
//! the current delegate. (The delegate re-key that ships with the journal
//! changes the delegate for its own reasons: #757's byte strings and #592's
//! loud key index.)
//!
//! # The completeness journal (freenet/river#763)
//!
//! An interrupted migration used to strand rooms: a tab closed after the first
//! per-room write left 1 of 4 rooms on every later load (freenet/river#760),
//! because the next load saw a non-empty successor and treated it as
//! authoritative, and the in-progress flag meant to catch that lived in
//! `localStorage`, which the gateway's sandboxed iframe does not have. The
//! journal (`freenet_migrate::MigrationJournal`, stored in the CURRENT delegate
//! under [`freenet_migrate::MIGRATION_JOURNAL_KEY`]) replaces every in-session
//! guess with durable arithmetic: each predecessor is pending until the walk
//! resolves it, and an import the walk or the sweep started is flagged before
//! its first write. `migration_journal.rs` drives it: `begin` on every load, the
//! journaled walk while the status is incomplete, `note_app_import_started`
//! before each sweep merge, and the identity-import gate.
//!
//! # The four constraints this adapter is built around
//!
//! 1. **A timeout is not an absence** (freenet-migrate#19 override — the
//!    crate's recommended semantics treat silence as "the predecessor does not
//!    have it"; we deliberately do not). Every channel round-trip is three-way:
//!    a reply (present or definitively absent), silence
//!    ([`RiverDelegateChannel`]'s `Ok(None)`), or transport failure. Silence
//!    and failure NEVER classify a key as absent: a probe that gets silence is
//!    `Unresponsive`, a fetch/marker round-trip that gets silence is an `Err`
//!    that aborts the predecessor, and the walk retries next page load.
//! 2. **Never return a short export** (freenet/river#592, #763). The journal
//!    believes a short export as permanently as a `Done` marker, so
//!    [`RiverPredecessorIo::fetch_secrets`] fails loudly rather than return a
//!    partial set. The legacy WASM's `get_key_index` swallows a decode error into
//!    an empty list (frozen, cannot be fixed there), and freenet-core reports a
//!    failed secret read as "not found" (freenet/freenet-core#5857), so the
//!    listing alone proves nothing. `fetch_secrets` therefore reads the RAW index
//!    twice (`GetRequest` on the index's own storage key), refuses one that does
//!    not decode or whose two reads disagree, refuses a listing that disagrees
//!    with it, refuses an absent index while a fixed key (`rooms_data`,
//!    `outbound_dms`) is present (every registered generation writes the index
//!    on every store), and refuses a listed key that answers "no value" (River
//!    has never sent a `DeleteRequest` in production, and the re-keyed delegate
//!    updates the index before removing a value, so a listed key with no value
//!    can only be a read fault). The fixed keys are still probed directly.
//! 3. **Never invent a second merge.** The writer routes every recovered room
//!    through `Rooms::merge_from_source` at the generation's source rank with
//!    the SHARED identity/tombstone rank registry — the same machinery the
//!    sweep hydrates through — so ranked tombstones (freenet/river#590) and
//!    identity conflicts (freenet/river#527) are handled identically, and the
//!    flush persists through `save_rooms_to_delegate`'s per-room CAS
//!    (`reconcile_room_present`). This is stronger than the never-clobber the
//!    crate's Union policy rests on: precedence comes from generation RANK,
//!    not arrival order.
//! 4. **Markers are durable, in the CURRENT delegate's KV store**, and follow
//!    the crate's three-rule marker contract (see
//!    [`RiverSuccessorIo::migration_marker`] / `record_marker`). ghostkeys
//!    deliberately went the opposite way (in-memory, page-lifetime) because
//!    sealing a predecessor there can strand late-arriving data. River's case
//!    is different: a legacy delegate's data is FROZEN after the re-key (only
//!    the current delegate is ever written), so sealing is safe — and durable
//!    markers are what stop the walk re-importing a predecessor on every load
//!    in the gateway iframe, where `localStorage` is unavailable (the
//!    `localStorage` seal the journal replaced never persisted there). The one
//!    exception, an old tab writing to the newest predecessor during a
//!    rollout, is covered by the post-resolve top-up (`top_up_from_with`). Do
//!    not copy either choice into another app without re-deriving which case
//!    applies.
//!    Marker keys HEX-ENCODE the predecessor delegate key: the delegate's
//!    `create_origin_key` runs the storage key through
//!    `String::from_utf8_lossy`, which maps every invalid byte to U+FFFD, so
//!    two raw 32-byte predecessor keys could alias to one marker slot.
//!
//! # Honest bounds
//!
//! * **Withholding is run-scoped** (freenet-migrate#15) and
//!   **`imported_total()` under-reports** (freenet-migrate#16) — acknowledged,
//!   not fixed here; the report is logged, never rendered as counts.
//! * **Union resurrects delete-by-absence data** in general; for River this is
//!   bounded because tombstones are RANKED observations (freenet/river#590): a
//!   room left in a newer generation carries a tombstone that outranks an older
//!   generation's `Present`, so the merge suppresses the resurrection.
//! * A walk-recovered room is merged and persisted, and its secrets are
//!   re-decrypted, but it is NOT run through the sweep's full hydrate
//!   orchestration (subscriptions, actions_state rebuild) — those complete on
//!   the next page load. Release 2 (sweep retirement) moves that orchestration
//!   into the import seam.
//! * `LegacyBridge` is ignored: River's successor store was never written by
//!   the crate's older generation-keyed `import_secrets_once` API, so there are
//!   no legacy generation-keyed markers to bridge.
//! * **Union shadowing (freenet-migrate#42) is bounded by the ranked merge,
//!   within a session and across one.** Under Union a newest generation that
//!   is refused (silent, or an empty answer after data) does not stop the walk,
//!   so an older generation's copy of a room can be imported and saved first.
//!   Precedence is generation RANK, not arrival order (pinned by
//!   `a_refused_newest_generation_does_not_shadow_it_within_the_session`), and
//!   the rank of a legacy-supplied identity is persisted in `rooms_meta`
//!   (`RoomsMeta::identity_sources`) and restored after the next load, so a
//!   newer generation answering on a later load still wins the identity
//!   conflict (`a_newer_generations_identity_wins_across_a_page_load`, #768
//!   review B3). Before that, the older `self_sk` reloaded at the current
//!   delegate's rank and the newer generation's signing key was lost for good.
//!   The same restored rank lets a newer generation's TOMBSTONE, answering
//!   on a later load, remove a room an older generation supplied
//!   (`a_newer_generations_tombstone_wins_across_a_page_load`). What is NOT
//!   bounded across a load: an older build's tab saving `rooms_meta` without
//!   the field (it drops it), and the losing identity of a conflict, which is
//!   discarded rather than archived (an older key the user had been posting
//!   as is gone once the newer generation's key wins).
//!   (The sweep no longer holds older generations back after a refusal: that
//!   blocked the walk it meant to hand them to, #768 round 2.)

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{LazyLock, Mutex};

use dioxus::logger::tracing::{error, info, warn};
use freenet_migrate::{
    begin_migration_journal, is_reserved_key, migrate_delegate_secrets_journaled,
    note_app_import_started, AppImport, DelegateLineageEntry, ItemWrite, JournalError,
    JournalStatus, JournaledMigrationReport, JournaledSuccessorIo, MarkerQuery,
    MigrationAuthorization, MigrationMarker, PredecessorSecretsIo, ProbeVerdict, RecoveredSecret,
    SecretPair, SecretSelectionPolicy, SuccessorSecretsIo, UnionAck, MIGRATION_JOURNAL_KEY,
    PRED_DONE_MARKER_VALUE_DATA, PRED_DONE_MARKER_VALUE_EMPTY,
};
use freenet_stdlib::prelude::{CodeHash, DelegateKey};
use river_core::chat_delegate::{
    CasStoreResult, ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg,
    OutboundDmStore,
};

use crate::components::app::chat_delegate::{
    self, parse_room_storage_key, DelegateAwaitOutcome, OUTBOUND_DMS_STORAGE_KEY, ROOMS_META_KEY,
    ROOMS_STORAGE_KEY,
};
use crate::room_data::{MergeAuthority, MergeRanks, RoomSlot, Rooms, RoomsMeta};

// ---------------------------------------------------------------------------
// Marker keys (current delegate's KV store)
// ---------------------------------------------------------------------------

/// Storage-key prefix for a predecessor's COMPLETION marker in the current
/// delegate's KV store. The suffix is the lowercase-hex predecessor delegate
/// key (see [`pred_done_marker_key`] for why hex is load-bearing).
pub(crate) const MIGRATE_PRED_DONE_PREFIX: &[u8] = b"__migrate_pred_done__:";
/// Storage-key prefix for a predecessor's IN-PROGRESS marker. Written before
/// the first item, upgraded to done only after every item and the flush landed,
/// so an interrupted run is re-walked.
pub(crate) const MIGRATE_PRED_WIP_PREFIX: &[u8] = b"__migrate_pred_wip__:";

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn marker_key(prefix: &[u8], predecessor: &DelegateKey) -> Vec<u8> {
    let mut k = prefix.to_vec();
    k.extend_from_slice(hex_lower(predecessor.bytes()).as_bytes());
    k
}

/// The completion-marker storage key for `predecessor`.
///
/// The predecessor key is HEX-ENCODED, never raw: the delegate's
/// `create_origin_key` builds its secret key via `String::from_utf8_lossy`,
/// which maps every invalid UTF-8 byte to U+FFFD — so two different raw
/// predecessor keys could collide on ONE marker slot, sealing a predecessor
/// that was never migrated. Hex is pure ASCII, so the lossy pass is the
/// identity and every predecessor gets a distinct slot.
pub(crate) fn pred_done_marker_key(predecessor: &DelegateKey) -> Vec<u8> {
    marker_key(MIGRATE_PRED_DONE_PREFIX, predecessor)
}

/// The in-progress-marker storage key for `predecessor` (same encoding rules
/// as [`pred_done_marker_key`]).
pub(crate) fn pred_wip_marker_key(predecessor: &DelegateKey) -> Vec<u8> {
    marker_key(MIGRATE_PRED_WIP_PREFIX, predecessor)
}

/// Whether `key` is one of THIS module's migration markers. Used to exclude
/// markers from a predecessor's fetched pairs: when the current delegate later
/// becomes a predecessor itself, its store contains these keys, and offering
/// them to the writer (or re-copying them forward) would forge migration state.
/// (The crate filters its OWN reserved `__fmigrate__` namespace; these keys are
/// deliberately a different, app-chosen format — see the module docs — so the
/// crate cannot know to filter them.)
pub(crate) fn is_migration_marker_key(key: &[u8]) -> bool {
    key.starts_with(MIGRATE_PRED_DONE_PREFIX) || key.starts_with(MIGRATE_PRED_WIP_PREFIX)
}

/// The current delegate's count of loads that skipped the walk under the
/// freenet/river#761 cadence (see `migration_journal::walk_decision`). River's
/// own bookkeeping, never migrated.
pub(crate) const WALK_CADENCE_KEY: &[u8] = b"__river_migration_walk_skips__";

/// The post-resolve top-up window's state (freenet/river#768 review B1,
/// round 2 item 8): [`TopUpWindow`]. River's own bookkeeping, never migrated.
pub(crate) const TOPUP_WINDOW_KEY: &[u8] = b"__river_migration_topup_window__";

/// Predecessors refused for a short index whose provable rooms were salvaged
/// (#768 round 3, S2/S3): CBOR `Vec<(predecessor delegate key, digest)>`, the
/// digest being BLAKE3 of the raw index bytes and the `rooms_meta` bytes. A
/// frozen predecessor whose digest is unchanged is refused again without its
/// room slots being downloaded. River's own bookkeeping, never migrated.
pub(crate) const SHORT_INDEX_KEY: &[u8] = b"__river_migration_short_index__";

/// What [`SHORT_INDEX_KEY`] holds.
pub(crate) type ShortIndexRecord = Vec<([u8; 32], [u8; 32])>;

/// The digest a [`SHORT_INDEX_KEY`] entry records.
pub(crate) fn short_index_digest(raw_index: &[u8], rooms_meta: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&(raw_index.len() as u64).to_le_bytes());
    h.update(raw_index);
    h.update(rooms_meta);
    *h.finalize().as_bytes()
}

/// The chat delegate's key index, as a plain `GetRequest` key. Every generation
/// stores the index under `<origin>:` + `::key_index`, and a `GetRequest` for
/// `key` reads `<origin>:` + `key`, so this reads the index's raw bytes
/// (`delegates/chat-delegate/src/{utils,models}.rs`). The index is CBOR, never
/// a versioning envelope, so no generation's `read_versioned` alters it.
pub(crate) const KEY_INDEX_STORAGE_KEY: &[u8] = b"::key_index";

/// Mirror of the delegate's stored `KeyIndex`
/// (`delegates/chat-delegate/src/context.rs`, unchanged since 2025-04).
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct RawKeyIndex {
    pub(crate) keys: Vec<ChatDelegateKey>,
}

/// Whether `key` is migration bookkeeping rather than user data: this module's
/// markers, the crate's reserved namespaces (the completeness journal is one,
/// [`is_reserved_key`]), the walk-cadence counter, or the index itself. Never
/// fetched from a predecessor, never imported, never counted as its data, and
/// never part of a room load. The hand-rolled sweep reads only room and fixed
/// keys, so it cannot copy these either (freenet/river#763).
pub(crate) fn is_migration_bookkeeping_key(key: &[u8]) -> bool {
    is_migration_marker_key(key)
        || is_reserved_key(key)
        || key == WALK_CADENCE_KEY
        || key == TOPUP_WINDOW_KEY
        || key == SHORT_INDEX_KEY
        || key == KEY_INDEX_STORAGE_KEY
        || key == KV_INTENT_STORAGE_KEY
}

/// The delegate's crash-recovery intent record, as a plain `GetRequest` key
/// (`delegates/chat-delegate/src/handlers.rs` `KV_INTENT_SUFFIX`). Never
/// listed; bookkeeping.
pub(crate) const KV_INTENT_STORAGE_KEY: &[u8] = b"::kv_intent";

// ---------------------------------------------------------------------------
// Transport seam
// ---------------------------------------------------------------------------

/// One request/response round-trip against a specific delegate (current or
/// legacy).
///
/// # Contract
///
/// * `Ok(Some(response))` — the delegate EXECUTED and replied.
/// * `Ok(None)` — no reply within the bound (10 s in production). The delegate
///   could not execute, is not registered on this node, or the request was
///   lost. **Silence, not absence** — see the module docs.
/// * `Err(_)` — the transport is broken (send failed), or the awaiting slot was
///   cancelled (another request re-registered the same correlation key).
///
/// Takes `&self` deliberately: `migrate_delegate_secrets` holds the
/// predecessor reader and the successor writer as two simultaneous `&mut`
/// borrows and BOTH need the transport, so a shared `&C` sits in both.
pub(crate) trait RiverDelegateChannel {
    /// Send `request` to `target` and await its reply.
    fn request(
        &self,
        target: &DelegateKey,
        request: ChatDelegateRequestMsg,
    ) -> impl Future<Output = Result<Option<ChatDelegateResponseMsg>, String>>;

    /// Issue several requests concurrently, returning outcomes in input order.
    ///
    /// This exists because "concurrent" must NOT mean "concurrent sends" on the
    /// production transport. A send takes the single `WEB_API` write borrow and
    /// holds it across `api.send().await`; polling a second request into its
    /// send while the first is suspended there is a `RefCell` double-borrow,
    /// which in single-threaded WASM is a panic rather than contention. Today
    /// that is survived only by `WebApi::send` happening not to yield for
    /// small payloads — true of every delegate request, but a property of the
    /// transport rather than of this code, and free to stop being true.
    ///
    /// **Deliberately has NO default.** The obvious default would be a
    /// `join_all` over `request` — which is precisely the unsafe shape this
    /// method exists to replace. With such a default, deleting
    /// [`NodeDelegateChannel`]'s override (an entirely plausible "the default
    /// already does this" cleanup) would silently return production to the
    /// overlapping-borrow fan-out while every test stayed green. Requiring
    /// each implementor to state its choice makes that a compile error.
    fn request_all(
        &self,
        requests: Vec<(DelegateKey, ChatDelegateRequestMsg)>,
    ) -> impl Future<Output = Vec<Result<Option<ChatDelegateResponseMsg>, String>>>;
}

/// The production transport: River's pending-request registry
/// (`PENDING_REQUESTS` oneshot side-table) over the node WebSocket.
///
/// Requests to the CURRENT delegate register under the bare correlation key
/// (what the response handler rebuilds for a current-delegate response);
/// requests to any other delegate register under the delegate-scoped key
/// (`legacy_scoped_correlation`), which the response handler rebuilds from the
/// RESPONDING delegate — so a reply from predecessor B can never resolve a
/// request sent to predecessor A.
///
/// `PENDING_REQUESTS` is single-waiter-per-key: registering a request silently
/// DROPS a prior waiter for the same correlation key. The crate's walk is
/// strictly sequential (one outstanding request at a time), which makes that
/// safe within the walk; the [`RiverPredecessorIo::prewarm`] burst is the one
/// concurrent phase and every one of its requests has a DISTINCT key (scoped by
/// target delegate). Both facts are pinned by
/// `walk_is_sequential_and_prewarm_keys_are_distinct` below. The hand-rolled
/// sweep can still collide with the walk on the same (delegate, key) — which is
/// why the walk driver (`migration_journal::walk_driver`) waits for the load to settle first —
/// and a collision degrades safely: the dropped waiter resolves `Err`
/// (cancelled), classifying the predecessor `Unresponsive`, and the durable
/// markers make the next page load retry it.
pub(crate) struct NodeDelegateChannel;

impl RiverDelegateChannel for NodeDelegateChannel {
    async fn request(
        &self,
        target: &DelegateKey,
        request: ChatDelegateRequestMsg,
    ) -> Result<Option<ChatDelegateResponseMsg>, String> {
        let pending = if chat_delegate::is_current_delegate_key(target) {
            chat_delegate::enqueue_delegate_request(request).await?
        } else {
            chat_delegate::enqueue_delegate_request_to(target.clone(), request).await?
        };
        match chat_delegate::await_delegate_response_outcome(pending).await {
            DelegateAwaitOutcome::Response(resp) => Ok(Some(resp)),
            // Timeout: the delegate never answered. Silence, NOT absence.
            DelegateAwaitOutcome::TimedOut => Ok(None),
            // Cancelled: our waiter was displaced by a concurrent request for
            // the same correlation key (the hand-rolled sweep). Not an answer
            // about the data at all — surface as a transport fault so the
            // driver records Unresponsive and a later run retries.
            DelegateAwaitOutcome::Cancelled => {
                Err("delegate response waiter was displaced by a concurrent request".to_string())
            }
        }
    }

    /// Enqueue every request SEQUENTIALLY, then await the replies together.
    ///
    /// Each enqueue confines the `WEB_API` write borrow to its own synchronous
    /// send and returns a `PendingDelegateRequest`, so no two borrows can
    /// overlap however the sends are scheduled; the concurrency lives entirely
    /// in the await phase, which holds no borrow. Same shape the startup
    /// per-room fan-out uses (freenet/river#417).
    ///
    /// The wire behaviour is unchanged: every request is still in flight before
    /// any reply is awaited.
    ///
    /// Scope, stated honestly: this removes the overlap WITHIN the pre-warm.
    /// It does NOT remove the cross-task case — while this is suspended inside
    /// `send().await`, a `migrate_legacy_per_room` task spawned from the
    /// message loop can call `send_delegate_request_to` and take the same
    /// borrow. That case is still survived only by `WebApi::send` not yielding
    /// for small payloads, exactly as before.
    async fn request_all(
        &self,
        requests: Vec<(DelegateKey, ChatDelegateRequestMsg)>,
    ) -> Vec<Result<Option<ChatDelegateResponseMsg>, String>> {
        let mut pending = Vec::with_capacity(requests.len());
        for (target, request) in requests {
            let enqueued = if chat_delegate::is_current_delegate_key(&target) {
                chat_delegate::enqueue_delegate_request(request).await
            } else {
                chat_delegate::enqueue_delegate_request_to(target, request).await
            };
            pending.push(enqueued);
        }

        let mut out = Vec::with_capacity(pending.len());
        let awaits = pending.into_iter().map(|enqueued| async move {
            match enqueued {
                Ok(p) => match chat_delegate::await_delegate_response_outcome(p).await {
                    DelegateAwaitOutcome::Response(resp) => Ok(Some(resp)),
                    DelegateAwaitOutcome::TimedOut => Ok(None),
                    DelegateAwaitOutcome::Cancelled => Err(
                        "delegate response waiter was displaced by a concurrent request"
                            .to_string(),
                    ),
                },
                Err(e) => Err(e),
            }
        });
        out.extend(futures::future::join_all(awaits).await);
        out
    }
}

// ---------------------------------------------------------------------------
// Lineage
// ---------------------------------------------------------------------------

/// Build the library's lineage from River's generated `LEGACY_DELEGATES` table.
///
/// The table is `(delegate_key, code_hash)` OLDEST-FIRST (the
/// `legacy_delegates.toml` authoring order), so the slice index is the
/// generation — the SAME scale `source_rank_for_delegate_key` uses for merge
/// ranking, which is what makes [`RecoveredSecret::generation`] directly usable
/// as the merge's `source_rank`. Keys are the STORED values, never re-derived:
/// V1's recorded key predates the standard `BLAKE3(code_hash)` derivation and
/// does not derive, which is exactly what `irregular_key` records.
pub(crate) fn river_delegate_lineage(table: &[([u8; 32], [u8; 32])]) -> Vec<DelegateLineageEntry> {
    table
        .iter()
        .enumerate()
        .map(
            |(generation, (delegate_key, code_hash))| DelegateLineageEntry {
                generation: generation as u32,
                code_hash: *code_hash,
                delegate_key: *delegate_key,
                // True for rows whose recorded key does not derive from the code
                // hash (River's V1). The walk never re-derives keys, so this is
                // metadata — but `predecessor_delegate_keys_checked` would assert
                // on it, so record it truthfully rather than hardcoding false.
                irregular_key: blake3::hash(code_hash).as_bytes() != delegate_key,
                note: "river legacy delegate",
            },
        )
        .collect()
}

/// The policy River migrates under: [`SecretSelectionPolicy::UnionAllGenerations`].
///
/// `NewestSnapshotWins` halts the walk at the first silent predecessor, and
/// silence is ORDINARY here — most of the 27 registered generations were never
/// installed on any given node, and an uninstalled predecessor is
/// indistinguishable from a broken one. Halting there would strand every older
/// generation's rooms (the freenet/river#204 failure class). Union's
/// delete-by-absence resurrection hazard is bounded for River by RANKED
/// tombstones (freenet/river#590) — see the module docs — and the
/// run-scoped-withholding hazard (freenet-migrate#15) is acknowledged by the
/// `UnionAck`, not covered.
pub(crate) fn river_secret_policy() -> SecretSelectionPolicy {
    SecretSelectionPolicy::UnionAllGenerations(
        UnionAck::i_understand_union_resurrects_deleted_by_absence_secrets(),
    )
}

// ---------------------------------------------------------------------------
// Predecessor side
// ---------------------------------------------------------------------------

/// What the concurrent pre-warm learned about one predecessor.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Prewarmed {
    /// Replied to `ListRequest` with its key index.
    Listed(Vec<ChatDelegateKey>),
    /// Replied with something other than a `ListResponse` — proves execution,
    /// but the index is unknown (fetch re-asks).
    Executed,
    /// No reply within the bound.
    Silent,
    /// No reply within the bound, and the node answered a request to it with a
    /// TYPED "missing delegate" (`DelegateError::Missing`, core 0.2.137+). Still
    /// not proof it holds nothing (core unregisters without deleting secrets);
    /// the walk treats it exactly like silence, and the journal counts it in the
    /// absent streak that drives the freenet/river#761 cadence.
    NotInstalled,
}

/// Legacy delegates the node has answered with a TYPED "missing delegate"
/// (`SynchronizerError::DelegateMissing`) since the last pre-warm began. Fed by
/// `freenet_synchronizer.rs`'s error arm; read when a probe times out.
///
/// Only the typed answer counts. An empty reply (core 0.2.136 and earlier) or a
/// timeout is silence, never "not installed", and the untyped "not found in
/// store" wording is not used either: the error channel carries no request id,
/// and only the typed variant names the delegate unambiguously.
static NOT_INSTALLED: LazyLock<Mutex<HashSet<[u8; 32]>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn not_installed() -> std::sync::MutexGuard<'static, HashSet<[u8; 32]>> {
    NOT_INSTALLED.lock().unwrap_or_else(|p| p.into_inner())
}

/// Record that the node answered a request to the legacy delegate `key` with a
/// typed "missing delegate" error.
pub(crate) fn note_legacy_delegate_not_installed(key: &DelegateKey) {
    if let Ok(bytes) = <[u8; 32]>::try_from(key.bytes()) {
        not_installed().insert(bytes);
    }
}

/// Classify a probe that got no reply: not installed if the node said so (typed)
/// since the pre-warm began, else plain silence.
fn silent_verdict(predecessor: &DelegateKey) -> Prewarmed {
    let typed_missing = <[u8; 32]>::try_from(predecessor.bytes())
        .map(|k| not_installed().contains(&k))
        .unwrap_or(false);
    if typed_missing {
        Prewarmed::NotInstalled
    } else {
        Prewarmed::Silent
    }
}

/// Reads predecessors through River's own delegate protocol.
///
/// `probe_executable` is a `ListRequest`: any reply proves the old WASM
/// executed on this node; 10 s of silence is `Ok(false)` (`Unresponsive`,
/// never "no data"). `fetch_secrets` reconstructs the pair list from the
/// requests the frozen legacy WASM already understands: the key index
/// (dynamic `room:<vk>` slots + `rooms_meta`) UNIONED with the fixed probes
/// `rooms_data` + `outbound_dms` — the union is load-bearing, see the module
/// docs.
///
/// # Concurrent pre-warm
///
/// The crate walks predecessors sequentially, and an unregistered predecessor
/// costs a full 10 s probe timeout — 27 generations would be ~4.5 minutes per
/// load. [`Self::prewarm`] fires ALL the probes concurrently up front (~10 s
/// wall-clock total), caches each predecessor's reachability (and its key
/// index, so the common case needs no second `ListRequest`), and
/// `probe_executable` answers from the cache.
pub(crate) struct RiverPredecessorIo<'a, C: RiverDelegateChannel> {
    channel: &'a C,
    prewarmed: HashMap<Vec<u8>, Prewarmed>,
    /// Predecessors already refused for a short index at this digest, whose
    /// provable rooms were salvaged ([`SHORT_INDEX_KEY`]).
    short_index_refused: ShortIndexRecord,
    /// Room slots a short-index refusal proved exist, to merge after the walk:
    /// `(predecessor, digest, [(storage key, value)])` (#768 round 3, S3).
    salvage: Vec<(DelegateKey, [u8; 32], Vec<SecretPair>)>,
    /// This walk's answers: whether each predecessor that exported holds a DM
    /// store (shared with the writer for [`newest_archive_holder`]).
    walk_answers: WalkAnswers,
}

/// Which predecessors exported in this walk, and whether each holds a DM
/// store (`outbound_dms`). Shared between the walk's reader and writer: the
/// walk runs newest-first, so a newer generation's answer is known before an
/// older one is imported.
pub(crate) type WalkAnswers = std::rc::Rc<std::cell::RefCell<HashMap<[u8; 32], bool>>>;

/// Whether an export holds a DM store: the archive "data" of #776. A
/// generation with rooms but no DM store never saved a DM store, so the user
/// archived or un-archived nothing in it (both save the store): it holds no
/// archive decision, and an older generation's archives may merge past it.
pub(crate) fn holds_dm_store(pairs: &[SecretPair]) -> bool {
    pairs
        .iter()
        .any(|(k, _)| k.as_slice() == OUTBOUND_DMS_STORAGE_KEY)
}

impl<'a, C: RiverDelegateChannel> RiverPredecessorIo<'a, C> {
    pub(crate) fn new(channel: &'a C) -> Self {
        Self {
            channel,
            prewarmed: HashMap::new(),
            short_index_refused: Vec::new(),
            salvage: Vec::new(),
            walk_answers: WalkAnswers::default(),
        }
    }

    /// Probe every predecessor's executability concurrently and cache the
    /// results. Transport errors are NOT cached — a later live probe surfaces
    /// them to the driver as `Unresponsive { error }`.
    ///
    /// Safe to run concurrently because every request here has a DISTINCT
    /// correlation key (each is scoped by its target delegate), so the
    /// single-waiter `PENDING_REQUESTS` map cannot drop one probe's waiter for
    /// another's.
    pub(crate) async fn prewarm(&mut self, predecessors: &[DelegateLineageEntry]) {
        // A shared `&C` is what each concurrent probe captures — `self` stays
        // free for the cache inserts after the join.
        let channel = self.channel;
        let targets: Vec<DelegateKey> = predecessors
            .iter()
            .map(|entry| DelegateKey::new(entry.delegate_key, CodeHash::new(entry.code_hash)))
            .collect();
        // Only a "missing delegate" answered to THIS burst classifies a
        // predecessor `NotInstalled`, so forget older evidence for its targets.
        {
            let mut seen = not_installed();
            for entry in predecessors {
                seen.remove(&entry.delegate_key);
            }
        }
        // `request_all`, NOT a `join_all` of `request` — on the production
        // transport the latter can poll a second probe into its send while the
        // first still holds the `WEB_API` write borrow across `send().await`,
        // which is a RefCell double-borrow panic in single-threaded WASM.
        let outcomes = channel
            .request_all(
                targets
                    .iter()
                    .map(|key| (key.clone(), ChatDelegateRequestMsg::ListRequest))
                    .collect(),
            )
            .await;
        // `request_all`'s "one outcome per request, in order" contract is
        // documentation only. A short vec would silently drop the tail
        // predecessors from the cache and degrade them to full 10 s live
        // probes, so say so loudly.
        //
        // Deliberately a `warn!` and NOT an assert. `[profile.release]` sets
        // `panic = 'abort'` and `safe_spawn_local` does not catch panics, so an
        // assert here would kill the whole UI — on the one code path whose
        // purpose is to not lose the user's rooms. What it would guard is
        // benign by comparison: `zip` truncates safely and `key_index` falls
        // back to a live probe, so a short vec is slower-but-correct. Trading
        // that for total app death is the wrong way round.
        if outcomes.len() != targets.len() {
            warn!(
                "request_all returned {} outcome(s) for {} request(s); tail \
                 predecessors will fall back to live probes",
                outcomes.len(),
                targets.len()
            );
        }
        for (key, outcome) in targets.iter().zip(outcomes) {
            let key_bytes = key.bytes().to_vec();
            let learned = match outcome {
                Ok(Some(ChatDelegateResponseMsg::ListResponse { keys })) => Prewarmed::Listed(keys),
                Ok(Some(_)) => Prewarmed::Executed,
                Ok(None) => silent_verdict(key),
                // Not cached: the live probe retries and reports the error.
                Err(_) => continue,
            };
            self.prewarmed.insert(key_bytes, learned);
        }
    }

    /// The predecessor's key index: from the pre-warm cache when it answered
    /// there, else a live `ListRequest`. `Err` on silence or a non-List reply —
    /// by the trait contract this runs only after `probe_executable` returned
    /// `Ok(true)`, so an index that cannot be read now is a fault to retry,
    /// NEVER an empty index.
    async fn key_index(
        &mut self,
        predecessor: &DelegateKey,
    ) -> Result<Vec<ChatDelegateKey>, String> {
        if let Some(Prewarmed::Listed(keys)) = self.prewarmed.get(predecessor.bytes()) {
            return Ok(keys.clone());
        }
        match self
            .channel
            .request(predecessor, ChatDelegateRequestMsg::ListRequest)
            .await?
        {
            Some(ChatDelegateResponseMsg::ListResponse { keys }) => Ok(keys),
            Some(other) => Err(format!("unexpected reply to legacy ListRequest: {other:?}")),
            None => Err("legacy ListRequest timed out after a positive probe".to_string()),
        }
    }

    /// The predecessor's RAW key index: `Ok(None)` when none is stored, `Err`
    /// when the read did not establish an answer or the stored bytes do not
    /// decode (freenet/river#592: the predecessor's own `ListResponse` turns an
    /// undecodable index into an empty list).
    async fn raw_key_index(
        &mut self,
        predecessor: &DelegateKey,
    ) -> Result<Option<(Vec<u8>, Vec<ChatDelegateKey>)>, String> {
        match self.get_key(predecessor, KEY_INDEX_STORAGE_KEY).await? {
            None => Ok(None),
            Some(bytes) => match ciborium::from_reader::<RawKeyIndex, _>(bytes.as_slice()) {
                Ok(index) => Ok(Some((bytes, index.keys))),
                Err(e) => Err(format!(
                    "the predecessor's stored key index does not decode ({e}); refusing a \
                     short export (freenet/river#592)"
                )),
            },
        }
    }

    /// GET one storage key from `predecessor`. Three-way:
    /// `Ok(Some(bytes))` present, `Ok(None)` DEFINITIVELY absent (the delegate
    /// executed and said so), `Err` unknown (silence, transport, or a
    /// mismatched/unexpected reply) — unknown NEVER reads as absent.
    async fn get_key(
        &mut self,
        predecessor: &DelegateKey,
        storage_key: &[u8],
    ) -> Result<Option<Vec<u8>>, String> {
        let request = ChatDelegateRequestMsg::GetRequest {
            key: ChatDelegateKey::new(storage_key.to_vec()),
        };
        match self.channel.request(predecessor, request).await? {
            Some(ChatDelegateResponseMsg::GetResponse { key, value }) => {
                if key.as_bytes() != storage_key {
                    // A reply about a different key can only be a
                    // mis-correlated answer to some other request; acting on
                    // it would attribute one key's bytes to another.
                    return Err(format!(
                        "legacy GetResponse key mismatch: asked {:?}, got {:?}",
                        String::from_utf8_lossy(storage_key),
                        String::from_utf8_lossy(key.as_bytes())
                    ));
                }
                Ok(value)
            }
            Some(other) => Err(format!("unexpected reply to legacy GetRequest: {other:?}")),
            None => Err(format!(
                "legacy GetRequest for {:?} timed out (silence is not absence)",
                String::from_utf8_lossy(storage_key)
            )),
        }
    }
}

/// The candidate storage keys to fetch from a predecessor, given its key
/// index: the listed keys UNIONED with the fixed probes, minus migration
/// bookkeeping ([`is_migration_bookkeeping_key`]: this module's markers, the
/// crate's reserved journal namespace, the cadence counter), ordered so
/// `rooms_meta` comes FIRST: the walk writes items in this order, and the
/// rooms land one at a time, each a render. With the order already merged
/// they appear in the user's order rather than in key-byte order until the
/// order arrives and the list jumps (freenet/river#802). That is safe because
/// a merged order keeps entries for rooms not yet in the map. The other thing
/// that once needed `rooms_meta` after the rooms, lowering the identity ranks
/// its `identity_sources` record, now runs at the predecessor's flush
/// ([`RiverSuccessorIo::flush_predecessor`]), after every item.
///
/// Pure, so the union rule is unit-testable and mutation-testable on its own.
pub(crate) fn candidate_keys(listed: &[ChatDelegateKey]) -> Vec<Vec<u8>> {
    // `rooms_meta` is probed ALWAYS (#768 review A4): every per-room-era save
    // writes it, so it is the witness that an absent or short index is a read
    // fault, not an empty delegate. First, so the walk merges the order before
    // the rooms (#802, see above).
    let mut keys: Vec<Vec<u8>> = vec![ROOMS_META_KEY.to_vec()];
    for k in listed {
        let b = k.as_bytes();
        if is_migration_bookkeeping_key(b) {
            continue;
        }
        if !keys.iter().any(|existing| existing == b) {
            keys.push(b.to_vec());
        }
    }
    // Fixed probes: the single-blob era's keys, asked even when unlisted
    // because the frozen legacy WASM reports a corrupt index as empty.
    for fixed in [ROOMS_STORAGE_KEY, OUTBOUND_DMS_STORAGE_KEY] {
        if !keys.iter().any(|existing| existing == fixed) {
            keys.push(fixed.to_vec());
        }
    }
    keys
}

impl<C: RiverDelegateChannel> PredecessorSecretsIo for RiverPredecessorIo<'_, C> {
    type Error = String;

    /// Executability preflight, answered from the pre-warm cache when
    /// available. Any reply counts as executed; only silence is `Ok(false)`.
    /// (The walk asks [`probe_verdict`](Self::probe_verdict); this stays
    /// consistent with it.)
    async fn probe_executable(&mut self, predecessor: &DelegateKey) -> Result<bool, Self::Error> {
        Ok(self.probe_verdict(predecessor).await? == ProbeVerdict::Executable)
    }

    /// The preflight with its third answer (freenet-migrate 0.7):
    /// [`ProbeVerdict::NotInstalled`] only for a TYPED "missing delegate" the
    /// node answered (see [`NOT_INSTALLED`]); an empty reply or a timeout is
    /// [`ProbeVerdict::NoReply`]. The walk treats both alike; the journal
    /// counts the first in the absent streak.
    async fn probe_verdict(
        &mut self,
        predecessor: &DelegateKey,
    ) -> Result<ProbeVerdict, Self::Error> {
        let learned = match self.prewarmed.get(predecessor.bytes()) {
            Some(cached) => cached.clone(),
            None => {
                // Cache miss (not pre-warmed, or the pre-warm probe errored):
                // live probe, so a transport error surfaces to the driver.
                let outcome = self
                    .channel
                    .request(predecessor, ChatDelegateRequestMsg::ListRequest)
                    .await?;
                let learned = match outcome {
                    Some(ChatDelegateResponseMsg::ListResponse { keys }) => Prewarmed::Listed(keys),
                    Some(_) => Prewarmed::Executed,
                    None => silent_verdict(predecessor),
                };
                self.prewarmed
                    .insert(predecessor.bytes().to_vec(), learned.clone());
                learned
            }
        };
        Ok(match learned {
            Prewarmed::Listed(_) | Prewarmed::Executed => ProbeVerdict::Executable,
            Prewarmed::Silent => ProbeVerdict::NoReply,
            Prewarmed::NotInstalled => ProbeVerdict::NotInstalled,
        })
    }

    /// Enumerate `predecessor`'s secrets, or fail: never a short list
    /// (freenet/river#592, #763; see the module docs, constraint 2).
    ///
    /// 1. The listing (pre-warm cache, else a live `ListRequest`).
    /// 2. The RAW index, read twice: `Err` if it does not decode, if the two
    ///    reads disagree, or if the listing disagrees with it. The predecessor
    ///    builds its listing from the same bytes, so a frozen predecessor
    ///    answers all three identically; a difference is a read fault
    ///    (freenet/freenet-core#5857 reports one as "not found").
    /// 3. Every listed key plus the fixed keys, each GET'd. A listed key that
    ///    answers "no value" is `Err`, and so is an absent index while a fixed
    ///    key is present: every registered generation adds a key to its index
    ///    when it stores it, so both can only be read faults.
    ///
    /// The cost of a wrong `Err` is a retry on the next load, and an
    /// interrupted import stays flagged so the identity-import gate holds; the
    /// cost of a wrong short `Ok` is the user's data, certified complete.
    async fn fetch_secrets(
        &mut self,
        predecessor: &DelegateKey,
    ) -> Result<Vec<SecretPair>, Self::Error> {
        let listed = self.key_index(predecessor).await?;
        let first = self.raw_key_index(predecessor).await?;
        let second = self.raw_key_index(predecessor).await?;
        if first.as_ref().map(|(bytes, _)| bytes) != second.as_ref().map(|(bytes, _)| bytes) {
            return Err(
                "two reads of the predecessor's key index disagree; a frozen predecessor \
                 does not change, so one read failed (freenet-core#5857)"
                    .to_string(),
            );
        }
        let index_absent = first.is_none();
        let first_bytes = first.as_ref().map(|(bytes, _)| bytes.clone());
        let indexed: Vec<ChatDelegateKey> = first.map(|(_, keys)| keys).unwrap_or_default();
        if indexed != listed {
            return Err(format!(
                "the predecessor listed {} key(s) but its stored index holds {}; refusing \
                 a short export (freenet/river#592)",
                listed.len(),
                indexed.len()
            ));
        }
        let raw_index = first_bytes.unwrap_or_default();
        // `rooms_meta` FIRST (#768 round 3, S2): it is what proves a short
        // index, and proving one must not first download every listed room.
        let meta_listed = listed.iter().any(|k| k.as_bytes() == ROOMS_META_KEY);
        let meta_value = self.get_key(predecessor, ROOMS_META_KEY).await?;
        match (&meta_value, meta_listed) {
            (Some(_), _) if index_absent => {
                return Err(format!(
                    "{:?} is stored but the predecessor has no key index; every \
                     generation indexes what it stores, so the index read failed",
                    String::from_utf8_lossy(ROOMS_META_KEY)
                ));
            }
            (Some(_), false) => {
                return Err(format!(
                    "{:?} is stored but its index does not list it; the index is \
                     short (freenet/river#592)",
                    String::from_utf8_lossy(ROOMS_META_KEY)
                ));
            }
            (None, true) => {
                return Err(format!(
                    "listed key {:?} answered \"no value\": a read fault, never a \
                     skip (freenet/river#763)",
                    String::from_utf8_lossy(ROOMS_META_KEY)
                ));
            }
            _ => {}
        }
        // The common #592 shape (#768 round 2 item 9): the short index was
        // rewritten by a later `rooms_meta` store, so it lists `rooms_meta`
        // and passes the checks above while room A's slot (and its `self_sk`)
        // is stored but unlisted. `rooms_meta` names the rooms the generation
        // held, and possibly more (since #802 its order keeps rooms that never
        // arrived): probe each named room slot that is not listed. Only a slot
        // that is actually stored proves the index short; one that answers
        // "no value" is skipped.
        if let Some(meta_bytes) = &meta_value {
            let digest = short_index_digest(&raw_index, meta_bytes);
            if self
                .short_index_refused
                .iter()
                .any(|(k, d)| k[..] == predecessor.bytes()[..] && *d == digest)
            {
                // Unchanged since it was refused and its rooms salvaged: a
                // frozen predecessor's short index stays short. Refuse
                // without downloading anything more (S2).
                return Err(
                    "the predecessor's index is short, unchanged since it was refused; \
                     its provable rooms were already recovered (freenet/river#592)"
                        .to_string(),
                );
            }
            let named = ciborium::from_reader::<RoomsMeta, _>(meta_bytes.as_slice())
                .map(|meta| rooms_named_by_meta(&meta))
                .unwrap_or_default();
            let mut proven: Vec<SecretPair> = Vec::new();
            for vk in named {
                let slot_key = chat_delegate::room_storage_key(&vk);
                if listed.iter().any(|k| k.as_bytes() == slot_key.as_slice()) {
                    continue;
                }
                if let Some(value) = self.get_key(predecessor, &slot_key).await? {
                    proven.push((slot_key, value));
                }
            }
            if !proven.is_empty() {
                let first_key = String::from_utf8_lossy(&proven[0].0).into_owned();
                // S3: the rooms it proves exist are recovered after the walk,
                // unflagged, through the ranked merge, while the generation
                // itself stays pending.
                self.salvage.push((predecessor.clone(), digest, proven));
                return Err(format!(
                    "{first_key:?} is named by rooms_meta and stored, but the index does \
                     not list it; the index is short (freenet/river#592)"
                ));
            }
        }
        let mut pairs: Vec<SecretPair> = Vec::new();
        for storage_key in candidate_keys(&listed) {
            if storage_key.as_slice() == ROOMS_META_KEY {
                // Read above; first, so its order merges before the rooms
                // (freenet/river#802).
                if let Some(value) = meta_value.clone() {
                    pairs.push((storage_key, value));
                }
                continue;
            }
            let is_listed = listed
                .iter()
                .any(|k| k.as_bytes() == storage_key.as_slice());
            match self.get_key(predecessor, &storage_key).await? {
                Some(_) if !is_listed && !index_absent => {
                    // A frozen generation's failed index read became an empty
                    // index that its next store rewrote as a one-key index
                    // (freenet/river#592): listing and index then agree, and
                    // are both short. A probed key that is stored but missing
                    // from an index that exists proves it (#768 review A4).
                    return Err(format!(
                        "{:?} is stored but its index does not list it; the index is \
                         short (freenet/river#592)",
                        String::from_utf8_lossy(&storage_key)
                    ));
                }
                Some(value) => {
                    if index_absent {
                        return Err(format!(
                            "{:?} is stored but the predecessor has no key index; every \
                             generation indexes what it stores, so the index read failed",
                            String::from_utf8_lossy(&storage_key)
                        ));
                    }
                    pairs.push((storage_key, value));
                }
                // An unlisted fixed probe that is not there: genuinely absent.
                None if !is_listed => {}
                None => {
                    return Err(format!(
                        "listed key {:?} answered \"no value\": a read fault, never a \
                         skip (freenet/river#763)",
                        String::from_utf8_lossy(&storage_key)
                    ));
                }
            }
        }
        if let Ok(key) = <[u8; 32]>::try_from(predecessor.bytes()) {
            self.walk_answers
                .borrow_mut()
                .insert(key, holds_dm_store(&pairs));
        }
        Ok(pairs)
    }
}

/// Every room a `rooms_meta` names: its order, its notification modes, its
/// cursor and its identity provenance, deduplicated.
pub(crate) fn rooms_named_by_meta(meta: &RoomsMeta) -> Vec<ed25519_dalek::VerifyingKey> {
    let mut named: Vec<ed25519_dalek::VerifyingKey> = Vec::new();
    let all = meta
        .room_order
        .iter()
        .chain(meta.notification_modes.keys())
        .chain(meta.current_room_key.iter())
        .chain(meta.identity_sources.iter().map(|(vk, _)| vk));
    for vk in all {
        if !named.contains(vk) {
            named.push(*vk);
        }
    }
    named
}

// ---------------------------------------------------------------------------
// Successor side
// ---------------------------------------------------------------------------

/// What one recovered pair decodes to. Pure classification, split from the
/// I/O so the mapping is unit-testable. (No `Debug` derive: the payload types
/// deliberately don't implement it — a `RoomData` debug-print would put
/// private keys in logs.)
pub(crate) enum RecoveredItem {
    /// A legacy single-blob `rooms_data` snapshot.
    RoomsBlob(Box<Rooms>),
    /// A per-room `room:<vk>` slot.
    Room(ed25519_dalek::VerifyingKey, RoomSlot),
    /// The `rooms_meta` view preferences.
    Meta(RoomsMeta),
    /// The outbound-DM plaintext cache.
    OutboundDms(Box<OutboundDmStore>),
    /// Migration bookkeeping (this module's markers, the crate's journal, the
    /// cadence counter: a future predecessor's store carries them) — never
    /// imported, never a failure. `fetch_secrets` already withholds them.
    MigrationMarker,
    /// A key this version of River does not understand — not ours to copy;
    /// the successor's state stands.
    Unrecognized,
    /// A recognized key whose value failed to decode. Permanently rejected:
    /// bytes that do not parse now will not parse later.
    Undecodable(&'static str, String),
}

/// Decode one recovered `(key, value)` pair into a [`RecoveredItem`].
pub(crate) fn classify_recovered(key: &[u8], value: &[u8]) -> RecoveredItem {
    if is_migration_bookkeeping_key(key) {
        return RecoveredItem::MigrationMarker;
    }
    if key == ROOMS_STORAGE_KEY {
        return match ciborium::from_reader::<Rooms, _>(value) {
            Ok(rooms) => RecoveredItem::RoomsBlob(Box::new(rooms)),
            Err(e) => RecoveredItem::Undecodable("rooms_data", e.to_string()),
        };
    }
    if key == OUTBOUND_DMS_STORAGE_KEY {
        return match ciborium::from_reader::<OutboundDmStore, _>(value) {
            Ok(store) => RecoveredItem::OutboundDms(Box::new(store)),
            Err(e) => RecoveredItem::Undecodable("outbound_dms", e.to_string()),
        };
    }
    if key == ROOMS_META_KEY {
        return match ciborium::from_reader::<RoomsMeta, _>(value) {
            Ok(meta) => RecoveredItem::Meta(meta),
            Err(e) => RecoveredItem::Undecodable("rooms_meta", e.to_string()),
        };
    }
    if let Some(vk) = parse_room_storage_key(key) {
        return match ciborium::from_reader::<RoomSlot, _>(value) {
            Ok(slot) => RecoveredItem::Room(vk, slot),
            Err(e) => RecoveredItem::Undecodable("room slot", e.to_string()),
        };
    }
    RecoveredItem::Unrecognized
}

/// Convert a decoded item into the single-source `Rooms` to merge, or `None`
/// for items that do not merge through the rooms path.
///
/// The legacy blob's `current_room_key` is STRIPPED: a legacy cursor is
/// years-stale by construction and must never dictate selection
/// (freenet/river#255 — the same rule `decide_current_room_restore` applies on
/// the sweep's path; `merge_from_source` does not touch the cursor, this makes
/// the invariant hold even if a future merge change does).
pub(crate) fn rooms_to_merge(item: &RecoveredItem) -> Option<Rooms> {
    match item {
        RecoveredItem::RoomsBlob(rooms) => {
            let mut rooms = (**rooms).clone();
            rooms.current_room_key = None;
            Some(rooms)
        }
        RecoveredItem::Room(vk, slot) => {
            let mut rooms = empty_rooms();
            match slot {
                RoomSlot::Present(room) => {
                    rooms.map.insert(*vk, (**room).clone());
                }
                RoomSlot::Tombstone => {
                    rooms.removed_rooms.insert(*vk);
                }
            }
            Some(rooms)
        }
        RecoveredItem::Meta(meta) => {
            let mut rooms = empty_rooms();
            rooms.room_order = meta.room_order.clone();
            rooms.notification_modes = meta.notification_modes.clone();
            // meta.current_room_key deliberately dropped (see above).
            Some(rooms)
        }
        _ => None,
    }
}

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

/// **The one merge** (constraint 3): fold a recovered single-source `Rooms`
/// into the in-memory set via `Rooms::merge_from_source` at the generation's
/// rank as an `OlderSnapshot` — the exact call the hand-rolled sweep's hydrate
/// makes, sharing `ranks` so the two paths' decisions are mutually consistent.
/// Ranked tombstones (freenet/river#590) and identity conflicts
/// (freenet/river#527) are therefore handled here without any new logic.
pub(crate) fn apply_recovered_rooms(
    current: &mut Rooms,
    incoming: Rooms,
    source_rank: u32,
    ranks: &mut MergeRanks,
) -> Result<(), String> {
    current.merge_from_source(incoming, source_rank, MergeAuthority::OlderSnapshot, ranks)
}

/// The successor-side import seam: what "apply this item" and "make it
/// durable" mean in River. Split from [`RiverSuccessorIo`] so the
/// classification/marker/tally behaviour is natively testable with a mock,
/// while the production impl ([`LiveImportSeam`]) touches the Dioxus signals
/// and the coalesced save.
pub(crate) trait RiverImportSeam {
    /// Merge a recovered single-source `Rooms` at `source_rank` (see
    /// [`apply_recovered_rooms`]).
    fn merge_rooms(
        &mut self,
        rooms: Rooms,
        source_rank: u32,
    ) -> impl Future<Output = Result<(), String>>;

    /// Merge a recovered outbound-DM store into the local cache. Its archive
    /// entries (`hidden_threads`) are merged only with `archive_rank`, which
    /// the walk passes only for the newest generation holding archive data
    /// ([`newest_archive_holder`], freenet/river#776). With `None` (an older
    /// generation, or the post-resolve top-up) only the DM entries are merged.
    fn merge_outbound_dms(
        &mut self,
        store: OutboundDmStore,
        archive_rank: Option<u32>,
    ) -> impl Future<Output = Result<(), String>>;

    /// The entries of `entries` the local outbound-DM cache does not already
    /// hold at least as fresh. The post-resolve top-up merges only these, so an
    /// unchanged predecessor costs no merge and no save (#768 round 3, N2).
    /// Default: all of them.
    fn new_outbound_entries(
        &mut self,
        entries: Vec<river_core::chat_delegate::OutboundDmEntry>,
    ) -> impl Future<Output = Vec<river_core::chat_delegate::OutboundDmEntry>> {
        std::future::ready(entries)
    }

    /// Persist everything merged so far; `Ok` only when the save REALLY
    /// landed. This is the durability boundary the crate's flush contract
    /// exists for — a failure here re-counts this predecessor's optimistic
    /// writes as failures and withholds its keys from older generations.
    fn flush(&mut self) -> impl Future<Output = Result<(), String>>;

    /// Lower the identity rank of rooms `from_rank` just supplied to the older
    /// rank their imported provenance records (`RoomsMeta::identity_sources`
    /// of the predecessor being imported; #768 round 2 item 4). Called after
    /// the predecessor's rooms merged. Default: nothing (a seam without ranks).
    fn lower_identity_ranks(
        &mut self,
        provenance: Vec<(ed25519_dalek::VerifyingKey, u32)>,
        from_rank: u32,
    ) -> impl Future<Output = ()> {
        let _ = (provenance, from_rank);
        std::future::ready(())
    }

    /// The rooms the successor already knows: present or tombstoned (owner
    /// key bytes). The post-resolve top-up fetches only rooms NOT in it. The
    /// default (nothing known) fetches every listed room.
    fn known_rooms(&mut self) -> impl Future<Output = HashSet<[u8; 32]>> {
        std::future::ready(HashSet::new())
    }
}

/// Production import seam: ranked merge into the `ROOMS` signal (inside
/// `crate::util::defer`, per the signal-safety rules, with the result returned
/// over a oneshot), DM hydrate via the same helpers the sweep uses, and flush
/// through the coalesced `save_rooms_to_delegate` /
/// `save_outbound_dms_to_delegate` — whose `CoalesceState::last_result`
/// propagates a covering save's real failure to us even when our own loop runs
/// zero iterations.
pub(crate) struct LiveImportSeam {
    rooms_dirty: bool,
    dms_dirty: bool,
    /// The load attempt this import runs on behalf of.
    attempt: u32,
}

impl LiveImportSeam {
    pub(crate) fn new(attempt: u32) -> Self {
        Self {
            rooms_dirty: false,
            dms_dirty: false,
            attempt,
        }
    }

    /// Refuse (retryably) when the current room set is no longer known
    /// complete for this import's load attempt: a reconnect or Retry began a
    /// new attempt, or something this attempt loaded failed. Checked before
    /// every merge and before the flush, so nothing is merged into, saved over,
    /// or sealed on top of an incomplete set (freenet/river#763 review A1,
    /// #595). A refusal is a retryable item or flush failure: the predecessor
    /// stays pending and the next load retries.
    fn ensure_room_set_complete(&self) -> Result<(), String> {
        if chat_delegate::room_set_complete(self.attempt) {
            Ok(())
        } else {
            Err(
                "the current room set is not known complete for this load attempt; \
                 not importing over it"
                    .to_string(),
            )
        }
    }
}

impl RiverImportSeam for LiveImportSeam {
    async fn merge_rooms(&mut self, rooms: Rooms, source_rank: u32) -> Result<(), String> {
        self.ensure_room_set_complete()?;
        self.rooms_dirty = true;
        let attempt = self.attempt;
        let (tx, rx) = futures::channel::oneshot::channel::<Result<(), String>>();
        // The predecessor's DMs, for the read-cutoff plan once merged
        // (freenet/river#801; see `dm_cutoff_plan`), as in the sweep's hydrate.
        let incoming_dms = crate::components::direct_messages::SnapshotDmSeen::of(&rooms);
        crate::util::defer(move || {
            // Re-checked at the moment of the mutation (#768 round 2 item 5):
            // a reconnect between the check above and this macrotask must not
            // let a superseded attempt write into ROOMS.
            if !chat_delegate::room_set_complete(attempt) {
                let _ = tx.send(Err(
                    "the load attempt changed before the merge ran; not merging".to_string(),
                ));
                return;
            }
            let result = crate::components::app::ROOMS.with_mut(|current| {
                let identity_watch =
                    crate::components::direct_messages::IdentityWatch::before(current);
                let merged = chat_delegate::with_identity_source_ranks(|ranks| {
                    apply_recovered_rooms(current, rooms, source_rank, ranks)
                });
                // Re-decrypt secrets for merged rooms (they are #[serde(skip)]),
                // as the sweep's hydrate does. Idempotent and cheap.
                for room_data in current.map.values_mut() {
                    room_data.repopulate_secrets_from_state();
                }
                crate::components::direct_messages::commit_dm_cutoff_plan(
                    crate::components::direct_messages::dm_cutoff_plan(
                        &incoming_dms,
                        &identity_watch,
                        crate::room_data::MergeAuthority::OlderSnapshot,
                        current,
                    ),
                );
                merged
            });
            // A ranked resurrection must be visible to the SAVE path, exactly
            // as in the sweep's hydrate: without `mark_room_rejoined` the
            // per-room CAS reconcile sees the delegate's tombstone, answers
            // AbortAdoptLeave, and the restored room stays tombstoned durably.
            let resurrected: Vec<[u8; 32]> = chat_delegate::with_identity_source_ranks(|ranks| {
                ranks.resurrected.drain().collect()
            });
            for room_key in resurrected {
                if let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&room_key) {
                    chat_delegate::mark_room_rejoined(vk);
                }
            }
            let _ = tx.send(result);
        });
        rx.await
            .map_err(|_| "rooms merge was dropped before running".to_string())?
    }

    async fn merge_outbound_dms(
        &mut self,
        store: OutboundDmStore,
        archive_rank: Option<u32>,
    ) -> Result<(), String> {
        self.ensure_room_set_complete()?;
        self.dms_dirty = true;
        // Same helpers the sweep's legacy DM arm uses, and idempotent per entry
        // — but the PUBLIC forms defer their signal writes through
        // `util::defer` (`setTimeout(0)`, a MACROTASK) and return immediately.
        //
        // `flush` -> `save_outbound_dms_to_delegate` reads `OUTBOUND_DMS`
        // SYNCHRONOUSLY, and the merge -> flush -> snapshot chain runs on
        // microtasks, which drain before any macrotask. Returning `Ok` here
        // without awaiting therefore let `flush` serialise the PRE-merge cache
        // while the driver went on to seal `Done { had_data: true }` — and the
        // marker guarantees the predecessor is never re-fetched, so the DMs
        // were lost permanently. Deterministic for a DM-only predecessor (no
        // rooms means no intervening network round-trip to let the timer fire),
        // and worst in the gateway iframe, where no `localStorage` means the
        // marker is the only authority.
        //
        // So run both writes inside ONE `defer` and await a oneshot signalled
        // from within it, exactly as `merge_rooms` above already does. The
        // `_now` helpers are the same bodies without their own `defer`, so this
        // keeps the single-`setTimeout` execution context the signal-safety
        // rules require rather than writing signals inline.
        let attempt = self.attempt;
        let (tx, rx) = futures::channel::oneshot::channel::<Result<(), String>>();
        crate::util::defer(move || {
            // Re-checked at the moment of the mutation (#768 round 2 item 5).
            if !chat_delegate::room_set_complete(attempt) {
                let _ = tx.send(Err(
                    "the load attempt changed before the DM merge ran; not merging".to_string(),
                ));
                return;
            }
            chat_delegate::hydrate_outbound_dms_cache_now(store.entries);
            // Archive entries only from the newest generation holding archive
            // data (#776); the caller decides (`newest_archive_holder`).
            if archive_rank.is_some() {
                chat_delegate::hydrate_hidden_dm_threads_now(store.hidden_threads);
            }
            let _ = tx.send(Ok(()));
        });
        rx.await
            .map_err(|_| "outbound-DM merge was dropped before running".to_string())?
    }

    async fn new_outbound_entries(
        &mut self,
        entries: Vec<river_core::chat_delegate::OutboundDmEntry>,
    ) -> Vec<river_core::chat_delegate::OutboundDmEntry> {
        chat_delegate::outbound_entries_not_held(entries)
    }

    async fn lower_identity_ranks(
        &mut self,
        provenance: Vec<(ed25519_dalek::VerifyingKey, u32)>,
        from_rank: u32,
    ) {
        // Inside `defer`, after the merge's own (awaited) defer, like the merges.
        let (tx, rx) = futures::channel::oneshot::channel::<()>();
        crate::util::defer(move || {
            chat_delegate::with_identity_source_ranks(|ranks| {
                crate::room_data::lower_identity_ranks(&mut ranks.identity, &provenance, from_rank)
            });
            let _ = tx.send(());
        });
        let _ = rx.await;
    }

    async fn known_rooms(&mut self) -> HashSet<[u8; 32]> {
        // Read inside `defer`, per the signal-safety rules, like the merges.
        let (tx, rx) = futures::channel::oneshot::channel::<HashSet<[u8; 32]>>();
        crate::util::defer(move || {
            let known: HashSet<[u8; 32]> = {
                use dioxus::prelude::ReadableExt;
                let rooms = crate::components::app::ROOMS.peek();
                rooms
                    .map
                    .keys()
                    .chain(rooms.removed_rooms.iter())
                    .map(|vk| vk.to_bytes())
                    .collect()
            };
            let _ = tx.send(known);
        });
        rx.await.unwrap_or_default()
    }

    async fn flush(&mut self) -> Result<(), String> {
        self.ensure_room_set_complete()?;
        if self.rooms_dirty {
            // The coalesced save's per-room CAS (`reconcile_room_present`) is
            // River's own import path to the delegate; its returned result is
            // the real durability outcome (`CoalesceState::last_result`).
            chat_delegate::save_rooms_to_delegate().await?;
            self.rooms_dirty = false;
        }
        if self.dms_dirty {
            chat_delegate::save_outbound_dms_to_delegate().await?;
            self.dms_dirty = false;
        }
        Ok(())
    }
}

/// River's own import path, as the library's successor writer.
///
/// Items are applied by MERGE at generation rank (see
/// [`apply_recovered_rooms`]), which subsumes the never-clobber rule Union's
/// newest-wins guarantee normally rests on: precedence comes from rank, not
/// arrival order, so re-offering and re-walking are idempotent. Writes are
/// BUFFERED (they land in the in-memory signals), so
/// [`SuccessorSecretsIo::flush_predecessor`] is load-bearing: it awaits the
/// coalesced delegate save and reports its real outcome.
pub(crate) struct RiverSuccessorIo<'a, C: RiverDelegateChannel, S: RiverImportSeam> {
    channel: &'a C,
    seam: S,
    /// The CURRENT delegate — where markers live.
    successor: DelegateKey,
    /// The lineage as `(delegate key, code hash)` in generation order, to read
    /// an imported `rooms_meta`'s identity provenance (#768 round 2 item 4).
    lineage_pairs: Vec<([u8; 32], [u8; 32])>,
    /// The journal as last loaded or stored in this walk: what
    /// [`newest_archive_holder`] reads, including predecessors resolved
    /// earlier in this same walk.
    journal: Option<Vec<u8>>,
    /// This walk's answers (see [`WalkAnswers`]).
    walk_answers: WalkAnswers,
    /// Identity provenance from a predecessor's `rooms_meta`, as
    /// `(provenance, generation)`, applied at its flush, once every room it
    /// supplied has merged (#768 round 2 item 4; deferred since #802 moved
    /// `rooms_meta` ahead of the rooms).
    pending_provenance: Vec<(Vec<(ed25519_dalek::VerifyingKey, u32)>, u32)>,
    /// Every `rooms_meta` order this walk has merged, as `(order, generation)`,
    /// folded newest generation first into one order and merged AGAIN at the
    /// flush of each predecessor that merged rooms. An order is merged before
    /// its rooms so they render in order (#802), but a
    /// room still tombstoned at that moment is skipped, and a slot of this or
    /// an OLDER predecessor can later resurrect it (a strictly newer rank than
    /// the tombstone, freenet/river#590). Re-merging after each predecessor's
    /// rooms gives that room back its position; `merge_room_order` only fills
    /// gaps, so for every other room it is a no-op. The resurrected room is
    /// placed by its nearest neighbour in the FOLDED order, which can be an
    /// older order's entry: a heuristic, not "the newest order wins" (a
    /// simulation in the #804 review found it closer to the user's order than
    /// merging the orders one by one). Positions only; nothing else is at
    /// stake. A tombstoned room skipped at the first merge is also missing as
    /// an anchor for its neighbours then, and placed entries never move.
    /// Only `rooms_meta` orders are collected, not the `room_order` inside a
    /// legacy `rooms_data` blob (its own rooms are placed by the same merge;
    /// a room it names but lacks is not re-placed). Residual: only THIS walk's
    /// orders are known, so a room resurrected in a later walk (or by the
    /// short-index salvage) after the generation whose order placed it was
    /// resolved earlier takes its own generation's position, or the tail.
    walk_orders: Vec<(Vec<ed25519_dalek::VerifyingKey>, u32)>,
    /// The predecessor being imported attempted to merge a room or tombstone
    /// (a slot or a `rooms_data` blob), even if the merge reported a per-room
    /// error, so its flush re-merges `walk_orders`. One that
    /// merged none cannot have resurrected anything, and re-merging would only
    /// dirty the rooms and cost a redundant save.
    rooms_merged: bool,
    /// Marker values read ahead by [`Self::prefetch_markers`], by storage key.
    /// Each is used by at most one [`Self::get_marker`] and is dropped when the
    /// key is written, so only a read that would otherwise have gone to the
    /// node a moment later is answered from here (freenet/river#809).
    prefetched_markers: HashMap<Vec<u8>, Option<Vec<u8>>>,
}

impl<'a, C: RiverDelegateChannel, S: RiverImportSeam> RiverSuccessorIo<'a, C, S> {
    pub(crate) fn new(channel: &'a C, seam: S, successor: DelegateKey) -> Self {
        Self {
            channel,
            seam,
            successor,
            lineage_pairs: Vec::new(),
            journal: None,
            walk_answers: WalkAnswers::default(),
            pending_provenance: Vec::new(),
            walk_orders: Vec::new(),
            rooms_merged: false,
            prefetched_markers: HashMap::new(),
        }
    }

    /// Read the done and in-progress markers of every predecessor in
    /// `lineage` concurrently, in waves of [`MARKER_PREFETCH_WAVE`], so that
    /// `begin` does not read them one round trip at a time (freenet/river#809:
    /// two reads per generation, 66 round trips for 33 generations, which held
    /// the first load after a re-key for about 8 s at 120 ms RTT).
    ///
    /// Called only before the first `begin` on a successor ([`begin_with`]).
    ///
    /// Only definitive answers are kept: a marker that is silent or errs here
    /// is read again, one at a time, when `begin` asks for it, and fails there
    /// exactly as it would have without the prefetch. Silence is still never
    /// read as absence.
    pub(crate) async fn prefetch_markers(&mut self, lineage: &[DelegateLineageEntry]) {
        let keys: Vec<Vec<u8>> = lineage
            .iter()
            .flat_map(|entry| {
                let predecessor =
                    DelegateKey::new(entry.delegate_key, CodeHash::new(entry.code_hash));
                [
                    pred_done_marker_key(&predecessor),
                    pred_wip_marker_key(&predecessor),
                ]
            })
            .collect();
        for wave in keys.chunks(MARKER_PREFETCH_WAVE) {
            // `request_all`, NOT a `join_all` of `request`: see `prewarm`.
            let outcomes = self
                .channel
                .request_all(
                    wave.iter()
                        .map(|key| {
                            (
                                self.successor.clone(),
                                ChatDelegateRequestMsg::GetRequest {
                                    key: ChatDelegateKey::new(key.clone()),
                                },
                            )
                        })
                        .collect(),
                )
                .await;
            for (key, outcome) in wave.iter().zip(outcomes) {
                if let Ok(Some(ChatDelegateResponseMsg::GetResponse {
                    key: reply_key,
                    value,
                })) = outcome
                {
                    if reply_key.as_bytes() == key.as_slice() {
                        self.prefetched_markers.insert(key.clone(), value);
                    }
                }
            }
        }
    }

    /// `Some(generation)` when `generation` is the newest predecessor holding
    /// archive data, by this walk's journal ([`newest_archive_holder`]).
    fn archive_rank(&self, generation: u32) -> Option<u32> {
        let journal = self
            .journal
            .as_deref()
            .and_then(|b| freenet_migrate::MigrationJournal::decode(b).ok());
        let not_installed = not_installed().clone();
        let answers = self.walk_answers.borrow().clone();
        newest_archive_holder(
            generation,
            &self.lineage_pairs,
            journal.as_ref(),
            &|k| answers.get(k).copied(),
            &|k| not_installed.contains(k),
        )
        .then_some(generation)
    }

    /// Know the lineage, so an imported `rooms_meta`'s identity provenance can
    /// be carried into the rank registry.
    pub(crate) fn with_lineage(mut self, lineage: &[DelegateLineageEntry]) -> Self {
        let mut entries: Vec<&DelegateLineageEntry> = lineage.iter().collect();
        entries.sort_by_key(|e| e.generation);
        self.lineage_pairs = entries
            .into_iter()
            .map(|e| (e.delegate_key, e.code_hash))
            .collect();
        self
    }

    /// GET a marker key from the CURRENT delegate. Three-way: value / definitive
    /// absence / `Err` for silence, transport failure, or an unexpected reply.
    /// An unreadable marker MUST be `Err` (→ `WriterUnavailable`, walk stops):
    /// guessing `None` would re-import a possibly-done predecessor, and
    /// guessing `Done` would silently skip one.
    async fn get_marker(&mut self, storage_key: Vec<u8>) -> Result<Option<Vec<u8>>, String> {
        if let Some(value) = self.prefetched_markers.remove(&storage_key) {
            return Ok(value);
        }
        let request = ChatDelegateRequestMsg::GetRequest {
            key: ChatDelegateKey::new(storage_key.clone()),
        };
        match self.channel.request(&self.successor, request).await? {
            Some(ChatDelegateResponseMsg::GetResponse { key, value }) => {
                if key.as_bytes() != storage_key.as_slice() {
                    return Err("marker GetResponse answered a different key".to_string());
                }
                Ok(value)
            }
            Some(other) => Err(format!("unexpected reply to marker GetRequest: {other:?}")),
            None => Err("marker GetRequest timed out (silence is not absence)".to_string()),
        }
    }

    /// `predecessor`'s marker pair, read per rule 1 ([`read_marker_pair`]):
    /// the wip marker is read whenever the done marker is absent or empty.
    pub(crate) async fn read_marker(
        &mut self,
        predecessor: &DelegateKey,
    ) -> Result<Option<MigrationMarker>, String> {
        let done = self.get_marker(pred_done_marker_key(predecessor)).await?;
        if done
            .as_deref()
            .is_some_and(|v| v != PRED_DONE_MARKER_VALUE_EMPTY)
        {
            return Ok(read_marker_pair(done.as_deref(), None));
        }
        let wip = self.get_marker(pred_wip_marker_key(predecessor)).await?;
        Ok(read_marker_pair(done.as_deref(), wip.as_deref()))
    }

    /// Store a value on the CURRENT delegate, awaiting the delegate's ack so it
    /// is durable when this returns (the crate's record_marker and
    /// store_journal contracts — never batched with items).
    async fn put_marker(&mut self, storage_key: Vec<u8>, value: &[u8]) -> Result<(), String> {
        self.prefetched_markers.remove(&storage_key);
        put_current(self.channel, &self.successor, storage_key, value).await
    }

    /// Record the in-progress marker without ever downgrading a stored
    /// `InProgress { saw_data: true }` (marker-contract rule 3), including one
    /// another session records between our read and our write: the check and
    /// the write are one compare-and-swap message to the delegate
    /// (`GetVersionedRequest` + `CasStoreRequest`), retried on a conflict.
    async fn put_wip_marker(
        &mut self,
        predecessor: &DelegateKey,
        saw_data: bool,
    ) -> Result<(), String> {
        let key = ChatDelegateKey::new(pred_wip_marker_key(predecessor));
        self.prefetched_markers.remove(key.as_bytes());
        let (mut current, mut generation) = match self
            .channel
            .request(
                &self.successor,
                ChatDelegateRequestMsg::GetVersionedRequest { key: key.clone() },
            )
            .await?
        {
            Some(ChatDelegateResponseMsg::GetVersionedResponse {
                key: reply_key,
                value,
                generation,
            }) if reply_key == key => (value, generation),
            Some(other) => {
                return Err(format!(
                    "unexpected reply to wip-marker GetVersionedRequest: {other:?}"
                ))
            }
            None => return Err("wip-marker GetVersionedRequest timed out".to_string()),
        };
        for _ in 0..WIP_MARKER_CAS_ATTEMPTS {
            let Some(value) = wip_marker_write(current.as_deref(), saw_data) else {
                // Already says what we would write, or says MORE (data): done.
                return Ok(());
            };
            match self
                .channel
                .request(
                    &self.successor,
                    ChatDelegateRequestMsg::CasStoreRequest {
                        key: key.clone(),
                        value: value.to_vec(),
                        expected_generation: generation,
                    },
                )
                .await?
            {
                Some(ChatDelegateResponseMsg::CasStoreResponse {
                    key: reply_key,
                    result,
                }) if reply_key == key => match result {
                    CasStoreResult::Stored { .. } => return Ok(()),
                    CasStoreResult::Conflict {
                        current_generation,
                        current_value,
                    } => {
                        current = current_value;
                        generation = current_generation;
                    }
                    CasStoreResult::Failed(e) => {
                        return Err(format!("delegate refused wip-marker store: {e}"))
                    }
                },
                Some(other) => {
                    return Err(format!(
                        "unexpected reply to wip-marker CasStoreRequest: {other:?}"
                    ))
                }
                None => return Err("wip-marker CasStoreRequest timed out".to_string()),
            }
        }
        Err("wip-marker store kept conflicting".to_string())
    }
}

/// How many marker reads [`RiverSuccessorIo::prefetch_markers`] keeps in
/// flight at once. Every delegate request on a node shares ONE admission queue
/// per priority tier, capped at 100 (`MAX_QUEUED_PER_CONTRACT`, freenet-core
/// `fair_queue.rs`), and the sweep's legacy burst lands in it at the same time;
/// a wave of 16 keeps well under the cap, as the room load's waves do
/// (`ROOM_LOAD_CONCURRENCY`).
pub(crate) const MARKER_PREFETCH_WAVE: usize = 16;
const _: () = assert!(MARKER_PREFETCH_WAVE > 0);

/// Compare-and-swap attempts for one in-progress marker write. Each conflict
/// means another session wrote it in between; a handful is plenty.
const WIP_MARKER_CAS_ATTEMPTS: u32 = 5;

/// What to write over a stored in-progress marker value when recording
/// `InProgress { saw_data }`: `None` when nothing must be written, because the
/// stored value already says it or says MORE (data over empty). Pure, so marker
/// rule 3 ("never downgrade a data wip marker") is unit-testable.
pub(crate) fn wip_marker_write(stored: Option<&[u8]>, saw_data: bool) -> Option<&'static [u8]> {
    let stored_data = stored.is_some_and(|v| v != PRED_DONE_MARKER_VALUE_EMPTY);
    match (stored, saw_data) {
        // Never downgrade data to empty (rule 3).
        (Some(_), false) if stored_data => None,
        (Some(v), false) if v == PRED_DONE_MARKER_VALUE_EMPTY => None,
        (Some(_), true) if stored_data => None,
        (_, true) => Some(PRED_DONE_MARKER_VALUE_DATA),
        (_, false) => Some(PRED_DONE_MARKER_VALUE_EMPTY),
    }
}

/// How a pair of stored marker values reads back (marker-contract rule 1):
/// `Done` wins, EXCEPT that a data in-progress marker beside an EMPTY `Done`
/// reads as `InProgress { saw_data: true }` — an import started after the empty
/// seal (the sweep's `note_app_import_started`, or a walk re-asking a
/// predecessor the journal knows holds data), so the empty seal is wrong.
/// Reading it as done would let a lost journal seal that import's missing items
/// away. Pure, so the rule is unit-testable.
pub(crate) fn read_marker_pair(done: Option<&[u8]>, wip: Option<&[u8]>) -> Option<MigrationMarker> {
    let wip_marker = wip.map(|v| MigrationMarker::InProgress {
        saw_data: v != PRED_DONE_MARKER_VALUE_EMPTY,
    });
    match done {
        Some(v) if v != PRED_DONE_MARKER_VALUE_EMPTY => {
            Some(MigrationMarker::Done { had_data: true })
        }
        Some(_) => match wip_marker {
            Some(MigrationMarker::InProgress { saw_data: true }) => wip_marker,
            _ => Some(MigrationMarker::Done { had_data: false }),
        },
        None => wip_marker,
    }
}

/// Store `value` under `key` on the CURRENT delegate (`successor`) and await the
/// delegate's acknowledgement. `Err` on a refusal, a mismatched reply, or
/// silence: a store is durable only once the delegate says so.
async fn put_current<C: RiverDelegateChannel>(
    channel: &C,
    successor: &DelegateKey,
    key: Vec<u8>,
    value: &[u8],
) -> Result<(), String> {
    let request = ChatDelegateRequestMsg::StoreRequest {
        key: ChatDelegateKey::new(key.clone()),
        value: value.to_vec(),
    };
    match channel.request(successor, request).await? {
        Some(ChatDelegateResponseMsg::StoreResponse {
            key: reply_key,
            result,
            ..
        }) if reply_key.as_bytes() == key.as_slice() => {
            result.map_err(|e| format!("delegate refused store: {e}"))
        }
        Some(other) => Err(format!("unexpected reply to StoreRequest: {other:?}")),
        None => Err("StoreRequest timed out (an unacknowledged store is not durable)".to_string()),
    }
}

/// GET `key` from the CURRENT delegate. Three-way: the value, a definitive
/// absence, or `Err` for silence, a transport fault or a mismatched reply —
/// never `Ok(None)` for a read that did not establish an answer.
async fn get_current<C: RiverDelegateChannel>(
    channel: &C,
    successor: &DelegateKey,
    key: Vec<u8>,
) -> Result<Option<Vec<u8>>, String> {
    let request = ChatDelegateRequestMsg::GetRequest {
        key: ChatDelegateKey::new(key.clone()),
    };
    match channel.request(successor, request).await? {
        Some(ChatDelegateResponseMsg::GetResponse {
            key: reply_key,
            value,
        }) if reply_key.as_bytes() == key.as_slice() => Ok(value),
        Some(other) => Err(format!("unexpected reply to GetRequest: {other:?}")),
        None => Err("GetRequest timed out (silence is not absence)".to_string()),
    }
}

impl<C: RiverDelegateChannel, S: RiverImportSeam> SuccessorSecretsIo
    for RiverSuccessorIo<'_, C, S>
{
    type Error = String;

    /// The marker contract (freenet-migrate 0.7), rule 1: a data in-progress
    /// marker beside an EMPTY done marker reads as in progress
    /// ([`read_marker_pair`]). Done and in-progress are separate keys, so the
    /// wip marker is read whenever the done marker is absent or empty.
    async fn migration_marker(
        &mut self,
        query: &MarkerQuery<'_>,
    ) -> Result<Option<MigrationMarker>, Self::Error> {
        // `query.legacy_bridge` is ignored: River never used the crate's older
        // generation-keyed `import_secrets_once` markers (module docs).
        self.read_marker(query.predecessor).await
    }

    /// The marker contract, rules 2 and 3: `InProgress` and `Done` are separate
    /// keys, so recording one never erases the other; and an `InProgress {
    /// saw_data: false }` never downgrades a stored data marker, even one
    /// another session writes concurrently (one compare-and-swap message,
    /// [`RiverSuccessorIo::put_wip_marker`]).
    async fn record_marker(
        &mut self,
        predecessor: &DelegateKey,
        marker: MigrationMarker,
    ) -> Result<(), Self::Error> {
        match marker {
            MigrationMarker::InProgress { saw_data } => {
                self.put_wip_marker(predecessor, saw_data).await
            }
            MigrationMarker::Done { had_data } => {
                let value = if had_data {
                    PRED_DONE_MARKER_VALUE_DATA
                } else {
                    PRED_DONE_MARKER_VALUE_EMPTY
                };
                self.put_marker(pred_done_marker_key(predecessor), value)
                    .await
            }
        }
    }

    async fn write_secret(&mut self, item: &RecoveredSecret<'_>) -> ItemWrite<Self::Error> {
        match classify_recovered(item.key, item.value) {
            decoded @ (RecoveredItem::RoomsBlob(_)
            | RecoveredItem::Room(..)
            | RecoveredItem::Meta(_)) => {
                let rooms = rooms_to_merge(&decoded).expect("rooms variants always convert");
                // `item.generation` is the lineage index, which is the SAME
                // scale `source_rank_for_delegate_key` ranks the sweep's
                // merges on (see `river_delegate_lineage`).
                // Set before the result is known: a merge that reports a
                // per-room error has still merged (and may have resurrected)
                // every other room (freenet/river#591), so the flush must
                // re-merge the orders. An extra order-only merge is harmless.
                if !matches!(decoded, RecoveredItem::Meta(_)) {
                    self.rooms_merged = true;
                }
                if let Err(e) = self.seam.merge_rooms(rooms, item.generation).await {
                    return ItemWrite::retryable(e);
                }
                // A predecessor's `rooms_meta` says which OLDER generation
                // supplied each room's identity: carry that into the rank
                // registry, or every room it holds would be re-stamped at its
                // own rank and a generation newer than the provenance would
                // lose (#768 round 2 item 4). Lowering touches only ranks the
                // rooms were merged at, so it must follow them: `rooms_meta` is
                // written FIRST (#802), so the lowering waits for the flush.
                if let RecoveredItem::Meta(meta) = &decoded {
                    if !meta.room_order.is_empty() {
                        self.walk_orders
                            .push((meta.room_order.clone(), item.generation));
                    }
                    let provenance = crate::room_data::identity_source_ranks(
                        &meta.identity_sources,
                        &self.lineage_pairs,
                    );
                    if !provenance.is_empty() {
                        self.pending_provenance.push((provenance, item.generation));
                    }
                }
                ItemWrite::Written
            }
            RecoveredItem::OutboundDms(store) => {
                match self
                    .seam
                    .merge_outbound_dms(*store, self.archive_rank(item.generation))
                    .await
                {
                    Ok(()) => ItemWrite::Written,
                    Err(e) => ItemWrite::retryable(e),
                }
            }
            // Never imported, never a failure: the successor's state stands.
            RecoveredItem::MigrationMarker | RecoveredItem::Unrecognized => {
                ItemWrite::AlreadyAuthoritative
            }
            // Bytes that do not decode now will not decode later. NOT
            // withheld from older generations (an older copy may parse).
            RecoveredItem::Undecodable(what, e) => {
                ItemWrite::permanent(format!("undecodable {what}: {e}"))
            }
        }
    }

    /// Re-merge the walk's room orders and lower the identity ranks the
    /// predecessor's `rooms_meta` recorded, now that every room it supplied has
    /// merged (see `write_secret`), then persist. The lowering precedes the
    /// save, which records the lowered ranks in this delegate's
    /// `identity_sources`.
    async fn flush_predecessor(&mut self, _predecessor: &DelegateKey) -> Result<(), Self::Error> {
        let provenances = std::mem::take(&mut self.pending_provenance);
        // The walk's orders once more, now that this predecessor's rooms
        // merged: a room one of its slots resurrected was skipped by an
        // earlier merge. Only a position is at stake, so a failure here is
        // logged and does not block the lowering or the flush (whose own
        // checks still decide whether this predecessor's import is durable).
        if std::mem::take(&mut self.rooms_merged) && !self.walk_orders.is_empty() {
            // Folded into ONE merge (each merge on the live seam re-renders
            // and re-decrypts every room), newest generation first, so where
            // two orders disagree the newer one's relative order is folded in
            // (`merge_room_order` keeps the first placement it sees). See
            // `walk_orders` for what the fold does and does not guarantee.
            let mut orders: Vec<&(Vec<ed25519_dalek::VerifyingKey>, u32)> =
                self.walk_orders.iter().collect();
            orders.sort_by_key(|(_, generation)| std::cmp::Reverse(*generation));
            let mut combined = Vec::new();
            for (order, _) in &orders {
                crate::room_data::merge_room_order(&mut combined, order, |_| false);
            }
            // Ranked at the newest order's generation; an order-only merge
            // records no rank, so the value only labels the call.
            let generation = orders[0].1;
            let mut rooms = empty_rooms();
            rooms.room_order = combined;
            if let Err(e) = self.seam.merge_rooms(rooms, generation).await {
                warn!("migration walk: re-merging the walk's room orders failed ({e})");
            }
        }
        for (provenance, from_rank) in provenances {
            self.seam.lower_identity_ranks(provenance, from_rank).await;
        }
        self.seam.flush().await
    }
}

/// River's import revision (freenet/river#763). A predecessor the journal
/// resolved with permanently rejected items under a LOWER revision reopens, so a
/// fixed decoder retries it. Raise it with any change to how the import path
/// decodes items.
///
/// 1: the delegate-message byte-string decoder (freenet/river#757), which ships
/// with the journal. No River journal existed under revision 0, but a rehearsal
/// build of the crate may have written one.
pub(crate) const RIVER_IMPORT_REVISION: u32 = 1;

impl<C: RiverDelegateChannel, S: RiverImportSeam> JournaledSuccessorIo
    for RiverSuccessorIo<'_, C, S>
{
    /// `Err` for a read that did not establish an answer (silence, a fault, a
    /// mismatched reply): `None` would make a later store start the journal
    /// afresh over a stored one.
    async fn load_journal(&mut self) -> Result<Option<Vec<u8>>, Self::Error> {
        let loaded = get_current(
            self.channel,
            &self.successor,
            MIGRATION_JOURNAL_KEY.to_vec(),
        )
        .await?;
        self.journal = loaded.clone();
        Ok(loaded)
    }

    /// Durable on return: waits for the delegate's acknowledgement.
    async fn store_journal(&mut self, journal: &[u8]) -> Result<(), Self::Error> {
        put_current(
            self.channel,
            &self.successor,
            MIGRATION_JOURNAL_KEY.to_vec(),
            journal,
        )
        .await?;
        self.journal = Some(journal.to_vec());
        Ok(())
    }

    // `journal_key` keeps the default (`MIGRATION_JOURNAL_KEY`), which every
    // walk and export withholds.

    fn import_revision(&self) -> u32 {
        RIVER_IMPORT_REVISION
    }
}

/// A seam for journal operations that never import an item (`begin` and the
/// sweep's `note_app_import_started`). Any call is a bug, reported as a
/// retryable failure rather than a write.
pub(crate) struct NoImports;

impl RiverImportSeam for NoImports {
    async fn merge_rooms(&mut self, _rooms: Rooms, _source_rank: u32) -> Result<(), String> {
        Err("a journal-only operation tried to import rooms".to_string())
    }
    async fn merge_outbound_dms(
        &mut self,
        _store: OutboundDmStore,
        _archive_rank: Option<u32>,
    ) -> Result<(), String> {
        Err("a journal-only operation tried to import outbound DMs".to_string())
    }
    async fn flush(&mut self) -> Result<(), String> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Journal operations (driven, one at a time, by `migration_journal.rs`)
// ---------------------------------------------------------------------------

/// `begin_migration_journal` against the current delegate `successor`: start
/// the journal (read off the markers) the first time, reopen a resolution a
/// lost flag left wrong, and return the status to decide the walk from.
pub(crate) async fn begin_with<C: RiverDelegateChannel>(
    channel: &C,
    successor: DelegateKey,
    lineage: &[DelegateLineageEntry],
) -> Result<JournalStatus, JournalError> {
    // Only a journal not yet stored (the first load after a re-key) reads
    // every marker, and that `begin` holds the sweep's first merge, so only it
    // is worth the prefetch. With a journal stored, `begin` runs off the
    // visible path and reads few markers; the prefetch there only competes
    // with the room load (about 0.2 s later in the rehearsal). A journal read
    // that is silent or errs skips the prefetch; `begin` meets the same
    // failure itself.
    let fresh = matches!(
        get_current(channel, &successor, MIGRATION_JOURNAL_KEY.to_vec()).await,
        Ok(None)
    );
    let mut writer = RiverSuccessorIo::new(channel, NoImports, successor);
    if fresh {
        writer.prefetch_markers(lineage).await;
    }
    begin_migration_journal(&mut writer, lineage, &river_secret_policy()).await
}

/// What the hand-rolled sweep does with the data it read from one predecessor,
/// decided by `note_app_import_started` (freenet/river#763).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SweepWrite {
    /// The import is flagged (journal + in-progress marker): merge.
    Merge,
    /// The journal says the walk already imported it, and nothing was
    /// recorded. Merge anyway, through the ranked, tombstone-safe
    /// `merge_from_source` path, unflagged: it fills a completed import that
    /// was partial (a short export, freenet-migrate#44 / river#592, or an old
    /// tab's writes during the rollout), and River's ranked tombstones keep a
    /// room deleted since from coming back.
    MergeUnflagged,
    /// Do not write: the journal could not record the import, or refused it.
    /// The predecessor is retried on a later load.
    Skip,
}

/// Map `note_app_import_started`'s answer to the sweep's action. Pure.
///
/// * `Flagged` → merge; `AlreadyImported` → merge unflagged (see
///   [`SweepWrite::MergeUnflagged`]).
/// * any `Err` → skip this predecessor on this load; it is retried on the
///   next. `NotImportable` is treated the same, never as permanent. (River
///   migrates under Union, so "skip every older one too", the snapshot-policy
///   rule, does not apply.)
/// * a variant this build does not know → do not write (`AppImport` is
///   `#[non_exhaustive]`).
pub(crate) fn sweep_write_for(result: &Result<AppImport, JournalError>) -> SweepWrite {
    match result {
        Ok(AppImport::Flagged) => SweepWrite::Merge,
        Ok(AppImport::AlreadyImported) => SweepWrite::MergeUnflagged,
        Ok(_) => SweepWrite::Skip,
        Err(_) => SweepWrite::Skip,
    }
}

/// `note_app_import_started` for the sweep's read of the legacy delegate whose
/// key bytes are `responding`, after the data is read and before it is merged.
///
/// The predecessor's `DelegateKey` is rebuilt from the LINEAGE entry, never
/// taken from the node's response, so it always matches the crate's
/// `lineage_key()` (a mismatch would be refused as "not in the lineage", which
/// is permanent). `items` counts in the walk's unit; see
/// `response_handler.rs::migrate_legacy_per_room`.
pub(crate) async fn note_sweep_import_with<C: RiverDelegateChannel>(
    channel: &C,
    successor: DelegateKey,
    lineage: &[DelegateLineageEntry],
    responding: &[u8],
    items: Option<u32>,
) -> SweepWrite {
    let Some(entry) = lineage
        .iter()
        .find(|e| e.delegate_key.as_slice() == responding)
    else {
        warn!("sweep: a delegate outside the legacy lineage answered; not merging its data");
        return SweepWrite::Skip;
    };
    let predecessor = DelegateKey::new(entry.delegate_key, CodeHash::new(entry.code_hash));
    let mut writer = RiverSuccessorIo::new(channel, NoImports, successor);
    let result = note_app_import_started(
        &mut writer,
        lineage,
        &predecessor,
        &river_secret_policy(),
        items,
    )
    .await;
    match &result {
        Ok(AppImport::Flagged) => info!(
            "sweep: import from legacy gen {} flagged in the migration journal",
            entry.generation
        ),
        Ok(AppImport::AlreadyImported) => info!(
            "sweep: legacy gen {} is already imported; merging unflagged through the ranked merge",
            entry.generation
        ),
        Ok(other) => warn!(
            "sweep: unknown journal answer {other:?} for legacy gen {}; not merging",
            entry.generation
        ),
        Err(JournalError::NotImportable(why)) => error!(
            "sweep: legacy gen {} refused as not importable ({why}); not merging. \
             River migrates under Union, so this should be unreachable: check that \
             the sweep's lineage matches the walk's",
            entry.generation
        ),
        Err(e) => warn!(
            "sweep: the migration journal could not flag legacy gen {} ({e}); not merging \
             it on this load",
            entry.generation
        ),
    }
    sweep_write_for(&result)
}

/// Read the freenet/river#761 walk-cadence counter from the current delegate.
/// `Err` when the read did not establish an answer.
pub(crate) async fn read_walk_cadence<C: RiverDelegateChannel>(
    channel: &C,
    successor: &DelegateKey,
) -> Result<u32, String> {
    match get_current(channel, successor, WALK_CADENCE_KEY.to_vec()).await? {
        None => Ok(0),
        Some(bytes) => <[u8; 4]>::try_from(bytes.as_slice())
            .map(u32::from_le_bytes)
            .map_err(|_| "walk-cadence counter is not 4 bytes".to_string()),
    }
}

/// Store the walk-cadence counter, awaiting the delegate's ack.
pub(crate) async fn store_walk_cadence<C: RiverDelegateChannel>(
    channel: &C,
    successor: &DelegateKey,
    skips: u32,
) -> Result<(), String> {
    put_current(
        channel,
        successor,
        WALK_CADENCE_KEY.to_vec(),
        &skips.to_le_bytes(),
    )
    .await
}

/// The post-resolve top-up window (#768 review B1, round 2 item 8): when it
/// began, how many top-ups have RUN in it (a load whose top-up did not run,
/// silent predecessor, error or gated attempt, is not counted), and the
/// BLAKE3 digest of the predecessor's raw key index when it was last read. A
/// changed index (an old tab added or removed a key) restarts the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TopUpWindow {
    pub(crate) started_ms: u64,
    pub(crate) runs: u32,
    pub(crate) index_digest: [u8; 32],
}

const TOPUP_WINDOW_LEN: usize = 8 + 4 + 32;

/// Read the top-up window from the current delegate; `Ok(None)` when it has
/// not started.
pub(crate) async fn read_topup_window<C: RiverDelegateChannel>(
    channel: &C,
    successor: &DelegateKey,
) -> Result<Option<TopUpWindow>, String> {
    match get_current(channel, successor, TOPUP_WINDOW_KEY.to_vec()).await? {
        None => Ok(None),
        Some(bytes) if bytes.len() == TOPUP_WINDOW_LEN => Ok(Some(TopUpWindow {
            started_ms: u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes")),
            runs: u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes")),
            index_digest: bytes[12..].try_into().expect("32 bytes"),
        })),
        Some(_) => Err(format!(
            "top-up window state is not {TOPUP_WINDOW_LEN} bytes"
        )),
    }
}

/// Store the top-up window, awaiting the delegate's ack.
pub(crate) async fn store_topup_window<C: RiverDelegateChannel>(
    channel: &C,
    successor: &DelegateKey,
    window: TopUpWindow,
) -> Result<(), String> {
    let mut bytes = Vec::with_capacity(TOPUP_WINDOW_LEN);
    bytes.extend_from_slice(&window.started_ms.to_le_bytes());
    bytes.extend_from_slice(&window.runs.to_le_bytes());
    bytes.extend_from_slice(&window.index_digest);
    put_current(channel, successor, TOPUP_WINDOW_KEY.to_vec(), &bytes).await
}

/// BLAKE3 of `predecessor`'s raw stored key index (one `GetRequest`): what
/// changes when an old tab adds or removes a key there. `Err` when the read
/// established no answer (silence is not "unchanged").
pub(crate) async fn predecessor_index_digest<C: RiverDelegateChannel>(
    channel: &C,
    predecessor: &DelegateKey,
) -> Result<[u8; 32], String> {
    let raw = RiverPredecessorIo::new(channel)
        .get_key(predecessor, KEY_INDEX_STORAGE_KEY)
        .await?;
    let mut hasher = blake3::Hasher::new();
    match raw {
        None => hasher.update(&[0]),
        Some(bytes) => hasher.update(&[1]).update(&bytes),
    };
    Ok(*hasher.finalize().as_bytes())
}

/// What one load's top-up did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TopUpOutcome {
    /// It ran, merging `merged` items; `window` is the recorded state after it.
    Ran { merged: u32, window: TopUpWindow },
    /// The window is over and the predecessor's index has not changed.
    Ended,
}

/// One load's post-resolve top-up from `predecessor` (the newest generation,
/// already resolved by the journal), driven by the persisted window:
/// read the predecessor's index digest, restart the window if it changed,
/// stop if the window is over, else top up and count the run. Any `Err`
/// (a silent predecessor, a failed merge, a failed write) counts nothing: the
/// next load retries.
pub(crate) async fn top_up_in_window_with<C: RiverDelegateChannel, S: RiverImportSeam>(
    channel: &C,
    seam: &mut S,
    successor: &DelegateKey,
    predecessor: &DelegateLineageEntry,
    now_ms: u64,
) -> Result<TopUpOutcome, String> {
    let key = DelegateKey::new(
        predecessor.delegate_key,
        CodeHash::new(predecessor.code_hash),
    );
    let digest = predecessor_index_digest(channel, &key).await?;
    let state = read_topup_window(channel, successor).await?;
    let window = match super::migration_journal::topup_decision(state, now_ms, digest) {
        super::migration_journal::TopUp::Ended => return Ok(TopUpOutcome::Ended),
        super::migration_journal::TopUp::Run { window } => window,
    };
    let merged = top_up_from_with(channel, seam, predecessor).await?;
    let window = TopUpWindow {
        runs: window.runs.saturating_add(1),
        ..window
    };
    store_topup_window(channel, successor, window).await?;
    Ok(TopUpOutcome::Ran { merged, window })
}

/// Top up from `predecessor` after the journal resolved it
/// (freenet/river#768 review B1): during the rollout an old tab keeps writing
/// to what is now a predecessor (the newest one, V33), and once the journal
/// resolves it nothing reads it again. This re-lists it and merges every room
/// the successor does not know at all (neither present nor tombstoned), plus
/// its outbound-DM cache, through the ranked, tombstone-safe merge, unflagged.
/// A room the user deleted in the new build is known (tombstoned), so it is
/// never fetched; and a ranked tombstone outranks it anyway.
///
/// Additive and idempotent, so it needs no journal flag: an interrupted top-up
/// leaves the data in the predecessor, and the next load in the window reads
/// it again. A silent predecessor is an `Err` (not a run). Returns the items
/// merged. [`top_up_in_window_with`] bounds it.
///
/// Only the newest predecessor is topped up: a room a tab on an OLDER build
/// creates after the journal resolved that older generation stays there (not
/// destroyed) and is not picked up (#768 round 3, N3).
pub(crate) async fn top_up_from_with<C: RiverDelegateChannel, S: RiverImportSeam>(
    channel: &C,
    seam: &mut S,
    predecessor: &DelegateLineageEntry,
) -> Result<u32, String> {
    let key = DelegateKey::new(
        predecessor.delegate_key,
        CodeHash::new(predecessor.code_hash),
    );
    let listed = match channel
        .request(&key, ChatDelegateRequestMsg::ListRequest)
        .await?
    {
        Some(ChatDelegateResponseMsg::ListResponse { keys }) => keys,
        Some(other) => return Err(format!("unexpected reply to top-up ListRequest: {other:?}")),
        None => return Err("the predecessor did not answer the top-up ListRequest".to_string()),
    };
    let known = seam.known_rooms().await;
    let mut reader = RiverPredecessorIo::new(channel);
    let mut merged = 0u32;
    for k in listed {
        let b = k.as_bytes();
        let wanted = match parse_room_storage_key(b) {
            Some(vk) => !known.contains(&vk.to_bytes()),
            None => b == OUTBOUND_DMS_STORAGE_KEY,
        };
        if !wanted || is_migration_bookkeeping_key(b) {
            continue;
        }
        let Some(value) = reader.get_key(&key, b).await? else {
            continue;
        };
        let item = classify_recovered(b, &value);
        if let RecoveredItem::OutboundDms(store) = item {
            // DM ENTRIES only. The predecessor's `hidden_threads` were imported
            // once by the walk; re-merging them on every load in the window
            // would re-archive a thread the user un-archived in the new build
            // (#768 round 2 item 6). An old tab's later archive is not carried
            // (a lost archive is recoverable; a re-hidden thread is a surprise).
            // And only entries the cache does not already hold: an unchanged
            // predecessor costs no merge and no save (#768 round 3, N2).
            let fresh = seam.new_outbound_entries(store.entries).await;
            if fresh.is_empty() {
                continue;
            }
            let entries_only = OutboundDmStore {
                entries: fresh,
                hidden_threads: Vec::new(),
            };
            // Entries only (`None`): the top-up never touches archive state.
            seam.merge_outbound_dms(entries_only, None).await?;
            merged += 1;
        } else if let Some(rooms) = rooms_to_merge(&item) {
            seam.merge_rooms(rooms, predecessor.generation).await?;
            merged += 1;
        }
    }
    if merged > 0 {
        seam.flush().await?;
    }
    Ok(merged)
}

/// The transport/seam-generic journaled walk: pre-warm the predecessors, then
/// drive `migrate_delegate_secrets_journaled` with River's reader and writer.
/// Production supplies [`NodeDelegateChannel`] + [`LiveImportSeam`]; the tests
/// supply mocks — everything in between (probe semantics, the loud fetch,
/// classification, ranked merge routing, markers, the journal) is EXACTLY the
/// code production runs.
pub(crate) async fn walk_with<C: RiverDelegateChannel, S: RiverImportSeam>(
    channel: &C,
    seam: S,
    successor: DelegateKey,
    lineage: &[DelegateLineageEntry],
) -> (JournaledMigrationReport, S) {
    let mut reader = RiverPredecessorIo::new(channel);
    // Unreadable: refuse short indexes the slow way this walk (cost only), and
    // write no record back (it would replace other predecessors' digests).
    let refused = read_short_index_record(channel, &successor).await.ok();
    reader.short_index_refused = refused.clone().unwrap_or_default();
    reader.prewarm(lineage).await;
    let mut writer = RiverSuccessorIo::new(channel, seam, successor.clone()).with_lineage(lineage);
    writer.walk_answers = reader.walk_answers.clone();
    let report = migrate_delegate_secrets_journaled(
        &mut writer,
        &mut reader,
        lineage,
        MigrationAuthorization::app_author_ack(),
        river_secret_policy(),
    )
    .await;
    let mut seam = writer.seam;
    let salvage = std::mem::take(&mut reader.salvage);
    if !salvage.is_empty() {
        salvage_short_index_rooms(channel, &mut seam, &successor, lineage, refused, salvage).await;
    }
    (report, seam)
}

/// Whether predecessor `generation` is the NEWEST generation holding DM
/// archive data, so its `hidden_threads` may be merged into the successor
/// (freenet/river#776). Un-archiving deletes an entry with no tombstone, so
/// merging an older generation's archives could bring back one a newer
/// generation deleted; only the newest data-holding generation's set is
/// merged, and nothing is ever removed.
///
/// Every NEWER generation must be known to hold no archive data, meaning no
/// DM store: it exported no DM store earlier in this walk (`answered`; rooms
/// alone do not count, see [`holds_dm_store`]), or it is resolved by the
/// journal and not data-bearing (the journal does not record which items,
/// so a generation resolved data-bearing in an EARLIER load counts as
/// holding archives, the conservative side), or pending but typed "not
/// installed" (by its
/// last walk's absent streak, or by this walk's pre-warm: a generation this
/// node never ran is never resolved). A newer generation that holds data, is
/// silent, was refused, or is missing from an unreadable journal makes this
/// one skip its archives. The cost of a wrong skip is a thread shown
/// un-archived; no message is lost. Pure.
pub(crate) fn newest_archive_holder(
    generation: u32,
    lineage_pairs: &[([u8; 32], [u8; 32])],
    journal: Option<&freenet_migrate::MigrationJournal>,
    answered: &dyn Fn(&[u8; 32]) -> Option<bool>,
    not_installed: &dyn Fn(&[u8; 32]) -> bool,
) -> bool {
    lineage_pairs
        .iter()
        .enumerate()
        .filter(|(g, _)| *g as u32 > generation)
        .all(|(_, (key, code))| {
            if let Some(held_data) = answered(key) {
                return !held_data;
            }
            let entry =
                journal.and_then(|j| j.entry(&DelegateKey::new(*key, CodeHash::new(*code))));
            match entry.map(|e| e.state) {
                Some(freenet_migrate::JournalState::Resolved(r)) => !r.is_data_bearing(),
                Some(freenet_migrate::JournalState::Pending { absent_streak, .. }) => {
                    absent_streak > 0 || not_installed(key)
                }
                // No entry (an unreadable journal) or a state this build does
                // not know: assume it may hold data.
                _ => false,
            }
        })
}

/// Recover the room slots a short-index refusal proved exist (#768 round 3,
/// S3): merge each through the ranked merge at its generation's rank,
/// unflagged (additive and idempotent, like the post-resolve top-up), while
/// the generation itself stays pending. Once a predecessor's slots merged and
/// flushed, record its digest ([`SHORT_INDEX_KEY`]) so later walks refuse it
/// without downloading them again (S2). A failure records nothing: the next
/// walk proves and salvages again.
async fn salvage_short_index_rooms<C: RiverDelegateChannel, S: RiverImportSeam>(
    channel: &C,
    seam: &mut S,
    successor: &DelegateKey,
    lineage: &[DelegateLineageEntry],
    record: Option<ShortIndexRecord>,
    salvage: Vec<(DelegateKey, [u8; 32], Vec<SecretPair>)>,
) {
    let readable = record.is_some();
    let mut record = record.unwrap_or_default();
    let before = record.len();
    for (pred, digest, pairs) in salvage {
        let Some(entry) = lineage
            .iter()
            .find(|e| e.delegate_key[..] == pred.bytes()[..])
        else {
            continue;
        };
        let mut ok = true;
        for (key, value) in pairs {
            if let Some(rooms) = rooms_to_merge(&classify_recovered(&key, &value)) {
                if let Err(e) = seam.merge_rooms(rooms, entry.generation).await {
                    warn!("short-index salvage: merge failed ({e}); the next walk retries");
                    ok = false;
                }
            }
        }
        if ok && seam.flush().await.is_ok() {
            info!(
                "short-index salvage: recovered the rooms legacy gen {} proves it holds",
                entry.generation
            );
            record.retain(|(k, _)| k[..] != pred.bytes()[..]);
            record.push((entry.delegate_key, digest));
        }
    }
    if !readable {
        warn!("short-index salvage: the stored record was unreadable; not replacing it");
    } else if record.len() != before || !record.is_empty() {
        if let Err(e) = store_short_index_record(channel, successor, &record).await {
            warn!("short-index salvage: could not record the refusal ({e})");
        }
    }
}

/// Read [`SHORT_INDEX_KEY`] (`Ok(empty)` when none is stored).
pub(crate) async fn read_short_index_record<C: RiverDelegateChannel>(
    channel: &C,
    successor: &DelegateKey,
) -> Result<ShortIndexRecord, String> {
    match get_current(channel, successor, SHORT_INDEX_KEY.to_vec()).await? {
        None => Ok(Vec::new()),
        Some(bytes) => ciborium::from_reader(bytes.as_slice())
            .map_err(|e| format!("short-index record does not decode: {e}")),
    }
}

/// Store [`SHORT_INDEX_KEY`], awaiting the delegate's ack.
pub(crate) async fn store_short_index_record<C: RiverDelegateChannel>(
    channel: &C,
    successor: &DelegateKey,
    record: &ShortIndexRecord,
) -> Result<(), String> {
    let mut bytes = Vec::new();
    ciborium::into_writer(record, &mut bytes).map_err(|e| e.to_string())?;
    put_current(channel, successor, SHORT_INDEX_KEY.to_vec(), &bytes).await
}

/// Log the walk's outcome. The report is METADATA ONLY and is never rendered
/// to the user as counts (`imported_total` legitimately reads 0 for a fully
/// successful recovery — freenet-migrate#16). A short retry after an
/// interrupted import (freenet-migrate#47) and permanently rejected items are
/// surfaced as warnings: that is River's telemetry for both.
pub(crate) fn log_migration_report(journaled: &JournaledMigrationReport) {
    let report = &journaled.report;
    for p in &report.predecessors {
        info!(
            "crate-migration predecessor gen {}: {:?}",
            p.generation(),
            p
        );
        if p.exported_fewer_after_data() || p.exported_nothing_after_data() {
            warn!(
                "crate-migration: legacy gen {} exported fewer items than an interrupted \
                 import saw; not believed, retried next load (freenet-migrate#44)",
                p.generation()
            );
        }
    }
    if report.any_unresponsive() {
        info!(
            "crate-migration: {} predecessor generation(s) did not answer; they stay \
             pending in the journal and are retried",
            report.unresponsive().count()
        );
    }
    if let Some(e) = &journaled.journal_error {
        warn!("crate-migration: migration journal error: {e}");
    }
    match &journaled.status {
        Some(status) => {
            let rejected = status.rejected_items();
            if rejected > 0 {
                warn!(
                    "crate-migration: {rejected} item(s) were permanently rejected by the \
                     import path and are NOT in the current delegate (they reopen when \
                     RIVER_IMPORT_REVISION rises)"
                );
            }
            info!(
                "crate-migration: journal {} (interrupted import: {})",
                if status.is_complete() {
                    "complete"
                } else {
                    "incomplete"
                },
                status.interrupted_import()
            );
        }
        None => warn!("crate-migration: journal status unknown after the walk"),
    }
}

/// Test doubles shared by this module's tests and by `migration_journal.rs`'s.
///
/// `MockChannel` models per-delegate key-value stores (with CAS generations and
/// the delegate's raw key index); it has NO correlation map and cannot model
/// displacement — `chat_delegate.rs`'s `two_driver_interleaving` tests drive the
/// real registry for that layer.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    // ---------------- scripted transport ----------------

    /// Yields once (waking itself) so concurrent mock requests genuinely
    /// overlap under `join_all`, making `max_in_flight` a real measurement.
    pub(crate) struct YieldOnce(pub(crate) bool);
    impl std::future::Future for YieldOnce {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    /// Scripted behaviour of one delegate on the mock node.
    #[derive(Default)]
    pub(crate) struct DelegateSim {
        /// KV store; GETs answer from it, Stores land in it.
        pub(crate) store: BTreeMap<Vec<u8>, Vec<u8>>,
        /// Override what `ListRequest` reports (None = the store's keys).
        /// `Some(vec![])` models the frozen legacy WASM's corrupt-index
        /// behaviour: an empty list even though the store holds data.
        pub(crate) listed: Option<Vec<Vec<u8>>>,
        /// The whole delegate never answers (not installed / cannot execute).
        pub(crate) silent: bool,
        /// GETs for these keys never answer (per-key silence).
        pub(crate) silent_get_keys: Vec<Vec<u8>>,
        /// `ListRequest` never answers (GETs still do).
        pub(crate) silent_list: bool,
        /// When a GET asks for `.0`, reply names key `.1` with value `.2` —
        /// a mis-correlated answer.
        pub(crate) wrong_key_reply: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
        /// What a GET of the raw key index (`KEY_INDEX_STORAGE_KEY`) answers.
        /// `None` = derived like the real delegate: the listing's keys encoded as
        /// a `KeyIndex`, or no index at all when the listing is empty.
        /// `Some(bytes)` overrides it (`Some(None)`: no index stored).
        pub(crate) raw_index: Option<Option<Vec<u8>>>,
        /// If set, the SECOND and later raw-index reads answer this instead (a
        /// read fault on a re-read).
        pub(crate) raw_index_reread: Option<Option<Vec<u8>>>,
        /// Raw-index reads answered so far.
        pub(crate) raw_index_reads: u32,
        /// The node answers every request to this delegate with a TYPED "missing
        /// delegate" (recorded as production's error arm records it) and no reply.
        pub(crate) not_installed: bool,
        /// Per-key CAS generation, bumped by every store (the delegate's
        /// versioning envelope, modelled).
        pub(crate) generations: BTreeMap<Vec<u8>, u64>,
        /// Another session's write landing between a reader's
        /// `GetVersionedRequest` and its `CasStoreRequest` for `.0`: applied
        /// (once) just before the first CAS for that key.
        pub(crate) cas_interleave: Option<(Vec<u8>, Vec<u8>)>,
        /// Every plain store is refused with this error (nothing is written).
        pub(crate) store_error: Option<String>,
        /// A plain store's acknowledgement names this key instead of the one
        /// stored (a mis-correlated answer).
        pub(crate) store_ack_key: Option<Vec<u8>>,
    }

    impl DelegateSim {
        /// The raw index the delegate holds: see [`DelegateSim::raw_index`].
        fn derived_raw_index(&self) -> Option<Vec<u8>> {
            let keys: Vec<Vec<u8>> = self
                .listed
                .clone()
                .unwrap_or_else(|| self.store.keys().cloned().collect());
            if keys.is_empty() {
                return None;
            }
            let mut out = Vec::new();
            ciborium::ser::into_writer(
                &RawKeyIndex {
                    keys: keys.into_iter().map(ChatDelegateKey::new).collect(),
                },
                &mut out,
            )
            .expect("encode index");
            Some(out)
        }
    }

    /// The successor (current delegate) every fixture uses.
    pub(crate) fn successor_key() -> DelegateKey {
        DelegateKey::new([0xAA; 32], CodeHash::new([0xAB; 32]))
    }

    #[derive(Default)]
    pub(crate) struct MockChannel {
        pub(crate) delegates: RefCell<HashMap<Vec<u8>, DelegateSim>>,
        pub(crate) in_flight: Cell<usize>,
        pub(crate) max_in_flight: Cell<usize>,
        /// `(target delegate key bytes, "list" | "get:<key>" | "store:<key>")`
        pub(crate) log: RefCell<Vec<(Vec<u8>, String)>>,
        /// Sequential round trips: each `request` outside a batch, and each
        /// `request_all` batch, counts one (freenet/river#809).
        pub(crate) round_trips: Cell<usize>,
        batching: Cell<bool>,
    }

    impl MockChannel {
        pub(crate) fn with_delegate(self, key: &DelegateKey, sim: DelegateSim) -> Self {
            self.delegates
                .borrow_mut()
                .insert(key.bytes().to_vec(), sim);
            self
        }
        /// Store `value` under `key` on `delegate` directly (fixture setup),
        /// creating the delegate if needed.
        /// Make `delegate` go silent (or answer again).
        pub(crate) fn silence(&self, delegate: &DelegateKey, silent: bool) {
            self.delegates
                .borrow_mut()
                .entry(delegate.bytes().to_vec())
                .or_default()
                .silent = silent;
        }
        pub(crate) fn put(&self, delegate: &DelegateKey, key: &[u8], value: &[u8]) {
            self.delegates
                .borrow_mut()
                .entry(delegate.bytes().to_vec())
                .or_default()
                .store
                .insert(key.to_vec(), value.to_vec());
        }
        pub(crate) fn reset_counters(&self) {
            self.in_flight.set(0);
            self.max_in_flight.set(0);
            self.log.borrow_mut().clear();
        }
        pub(crate) fn stored(&self, delegate: &DelegateKey, key: &[u8]) -> Option<Vec<u8>> {
            self.delegates
                .borrow()
                .get(delegate.bytes())
                .and_then(|sim| sim.store.get(key).cloned())
        }
        pub(crate) fn requests_to_predecessors(&self, kind: &str) -> usize {
            let succ = successor_key().bytes().to_vec();
            self.log
                .borrow()
                .iter()
                .filter(|(target, what)| *target != succ && what.starts_with(kind))
                .count()
        }
        pub(crate) fn answer(
            &self,
            target: &DelegateKey,
            request: ChatDelegateRequestMsg,
        ) -> Result<Option<ChatDelegateResponseMsg>, String> {
            let mut delegates = self.delegates.borrow_mut();
            let Some(sim) = delegates.get_mut(target.bytes()) else {
                // Unknown delegate: the node has nothing registered — silence.
                self.log
                    .borrow_mut()
                    .push((target.bytes().to_vec(), "unknown".into()));
                return Ok(None);
            };
            if sim.not_installed {
                self.log
                    .borrow_mut()
                    .push((target.bytes().to_vec(), "not-installed".into()));
                note_legacy_delegate_not_installed(target);
                return Ok(None);
            }
            match request {
                ChatDelegateRequestMsg::ListRequest => {
                    self.log
                        .borrow_mut()
                        .push((target.bytes().to_vec(), "list".into()));
                    if sim.silent || sim.silent_list {
                        return Ok(None);
                    }
                    let keys = sim
                        .listed
                        .clone()
                        .unwrap_or_else(|| sim.store.keys().cloned().collect());
                    Ok(Some(ChatDelegateResponseMsg::ListResponse {
                        keys: keys.into_iter().map(ChatDelegateKey::new).collect(),
                    }))
                }
                ChatDelegateRequestMsg::GetRequest { key } => {
                    let kb = key.as_bytes().to_vec();
                    self.log.borrow_mut().push((
                        target.bytes().to_vec(),
                        format!("get:{}", String::from_utf8_lossy(&kb)),
                    ));
                    if sim.silent || sim.silent_get_keys.contains(&kb) {
                        return Ok(None);
                    }
                    if let Some((asked, reply_key, reply_value)) = &sim.wrong_key_reply {
                        if *asked == kb {
                            return Ok(Some(ChatDelegateResponseMsg::GetResponse {
                                key: ChatDelegateKey::new(reply_key.clone()),
                                value: Some(reply_value.clone()),
                            }));
                        }
                    }
                    if kb == KEY_INDEX_STORAGE_KEY {
                        sim.raw_index_reads += 1;
                        let value = match (&sim.raw_index_reread, sim.raw_index_reads) {
                            (Some(reread), n) if n > 1 => reread.clone(),
                            _ => sim
                                .raw_index
                                .clone()
                                .unwrap_or_else(|| sim.derived_raw_index()),
                        };
                        return Ok(Some(ChatDelegateResponseMsg::GetResponse { key, value }));
                    }
                    Ok(Some(ChatDelegateResponseMsg::GetResponse {
                        key,
                        value: sim.store.get(&kb).cloned(),
                    }))
                }
                ChatDelegateRequestMsg::StoreRequest { key, value } => {
                    let kb = key.as_bytes().to_vec();
                    self.log.borrow_mut().push((
                        target.bytes().to_vec(),
                        format!("store:{}", String::from_utf8_lossy(&kb)),
                    ));
                    if sim.silent {
                        return Ok(None);
                    }
                    let value_size = value.len();
                    if let Some(e) = &sim.store_error {
                        return Ok(Some(ChatDelegateResponseMsg::StoreResponse {
                            key,
                            value_size: 0,
                            result: Err(e.clone()),
                        }));
                    }
                    *sim.generations.entry(kb.clone()).or_default() += 1;
                    sim.store.insert(kb, value);
                    let ack = sim
                        .store_ack_key
                        .clone()
                        .map(ChatDelegateKey::new)
                        .unwrap_or(key);
                    Ok(Some(ChatDelegateResponseMsg::StoreResponse {
                        key: ack,
                        value_size,
                        result: Ok(()),
                    }))
                }
                ChatDelegateRequestMsg::GetVersionedRequest { key } => {
                    let kb = key.as_bytes().to_vec();
                    self.log.borrow_mut().push((
                        target.bytes().to_vec(),
                        format!("getv:{}", String::from_utf8_lossy(&kb)),
                    ));
                    if sim.silent {
                        return Ok(None);
                    }
                    Ok(Some(ChatDelegateResponseMsg::GetVersionedResponse {
                        value: sim.store.get(&kb).cloned(),
                        generation: sim.generations.get(&kb).copied().unwrap_or(0),
                        key,
                    }))
                }
                ChatDelegateRequestMsg::CasStoreRequest {
                    key,
                    value,
                    expected_generation,
                } => {
                    let kb = key.as_bytes().to_vec();
                    self.log.borrow_mut().push((
                        target.bytes().to_vec(),
                        format!("cas:{}", String::from_utf8_lossy(&kb)),
                    ));
                    if sim.silent {
                        return Ok(None);
                    }
                    if sim.cas_interleave.as_ref().is_some_and(|(k, _)| *k == kb) {
                        let (k, v) = sim.cas_interleave.take().expect("checked");
                        *sim.generations.entry(k.clone()).or_default() += 1;
                        sim.store.insert(k, v);
                    }
                    let current = sim.generations.get(&kb).copied().unwrap_or(0);
                    let result = if current == expected_generation {
                        sim.store.insert(kb.clone(), value);
                        sim.generations.insert(kb, current + 1);
                        CasStoreResult::Stored {
                            generation: current + 1,
                        }
                    } else {
                        CasStoreResult::Conflict {
                            current_generation: current,
                            current_value: sim.store.get(&kb).cloned(),
                        }
                    };
                    Ok(Some(ChatDelegateResponseMsg::CasStoreResponse {
                        key,
                        result,
                    }))
                }
                other => Err(format!("mock: unsupported request {other:?}")),
            }
        }
    }

    impl RiverDelegateChannel for MockChannel {
        async fn request(
            &self,
            target: &DelegateKey,
            request: ChatDelegateRequestMsg,
        ) -> Result<Option<ChatDelegateResponseMsg>, String> {
            if !self.batching.get() {
                self.round_trips.set(self.round_trips.get() + 1);
            }
            self.in_flight.set(self.in_flight.get() + 1);
            self.max_in_flight
                .set(self.max_in_flight.get().max(self.in_flight.get()));
            YieldOnce(false).await;
            let out = self.answer(target, request);
            self.in_flight.set(self.in_flight.get() - 1);
            out
        }

        /// The double fans out naively ON PURPOSE: it holds no shared
        /// `WEB_API` borrow, so the hazard `NodeDelegateChannel::request_all`
        /// guards against cannot exist here, and keeping the concurrency
        /// observable is what lets `max_in_flight` assert the pre-warm really
        /// does probe every predecessor at once.
        ///
        /// Note what this means: these assertions measure the DOUBLE's shape,
        /// never production's. The production override is pinned separately,
        /// by source scrape.
        async fn request_all(
            &self,
            requests: Vec<(DelegateKey, ChatDelegateRequestMsg)>,
        ) -> Vec<Result<Option<ChatDelegateResponseMsg>, String>> {
            self.round_trips.set(self.round_trips.get() + 1);
            self.batching.set(true);
            let out = futures::future::join_all(
                requests
                    .iter()
                    .map(|(target, request)| self.request(target, request.clone())),
            )
            .await;
            self.batching.set(false);
            out
        }
    }

    /// A real journal status in which every pending predecessor answered "not
    /// installed" on `streak` consecutive journaled walks and nothing started:
    /// what `pending_all_absent` needs, reachable only through walks.
    pub(crate) fn all_absent_status(streak: u32) -> JournalStatus {
        // Keys no other test, and no other CALL of this function, uses:
        // `NOT_INSTALLED` is process-global, and a concurrent walk's pre-warm
        // clears the evidence for the keys it probes. Two tests sharing one
        // fixed key pair raced (the streak read 0 in about a fifth of runs
        // when both ran), so every call takes a fresh pair.
        static NEXT_PAIR: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0x40);
        let n = NEXT_PAIR.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
        let table = [
            ([n; 32], [n.wrapping_add(0x80); 32]),
            ([n + 1; 32], [n.wrapping_add(0x81); 32]),
        ];
        let lineage = river_delegate_lineage(&table);
        let mut channel = MockChannel::default();
        for (key, code) in table {
            let sim = DelegateSim {
                not_installed: true,
                ..Default::default()
            };
            channel = channel.with_delegate(&DelegateKey::new(key, CodeHash::new(code)), sim);
        }
        channel = channel.with_delegate(&successor_key(), DelegateSim::default());
        let mut status = None;
        for _ in 0..streak {
            let (report, _) = futures::executor::block_on(walk_with(
                &channel,
                NoImports,
                successor_key(),
                &lineage,
            ));
            status = report.status;
        }
        status.expect("journaled walk status")
    }

    /// A status after `walks` journaled walks over a 2-generation lineage whose
    /// generations are installed but never answer (untyped silence: a delegate
    /// that cannot execute, or a node that predates typed absence).
    pub(crate) fn silent_status(walks: u32) -> JournalStatus {
        static NEXT_PAIR: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0x10);
        let n = NEXT_PAIR.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
        let table = [
            ([n; 32], [n.wrapping_add(0x80); 32]),
            ([n + 1; 32], [n.wrapping_add(0x81); 32]),
        ];
        let lineage = river_delegate_lineage(&table);
        let mut channel = MockChannel::default();
        for (key, code) in table {
            let sim = DelegateSim {
                silent: true,
                ..Default::default()
            };
            channel = channel.with_delegate(&DelegateKey::new(key, CodeHash::new(code)), sim);
        }
        channel = channel.with_delegate(&successor_key(), DelegateSim::default());
        let mut status = None;
        for _ in 0..walks {
            let (report, _) = futures::executor::block_on(walk_with(
                &channel,
                NoImports,
                successor_key(),
                &lineage,
            ));
            status = report.status;
        }
        status.expect("journaled walk status")
    }
}

// ---------------------------------------------------------------------------
// Tests (native). The scenario tests drive the REAL `freenet_migrate` driver
// through `walk_with` with a scripted transport and an in-memory seam, so
// everything between the transport and the signals — probe semantics, the
// union fetch, classification, ranked-merge routing, marker bookkeeping — is
// exactly the code production runs.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;
    use ed25519_dalek::{SigningKey, VerifyingKey};
    use freenet_migrate::{
        DelegateMigrationReport, PredecessorMigration, WriterFailure, WriterStage,
    };
    use futures::executor::block_on;

    // ---------------- fixtures ----------------

    fn vk(seed: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    fn cbor<T: serde::Serialize>(v: &T) -> Vec<u8> {
        let mut out = Vec::new();
        ciborium::ser::into_writer(v, &mut out).expect("serialize fixture");
        out
    }

    fn room_with_identity(owner: VerifyingKey, sk_seed: u8) -> crate::room_data::RoomData {
        let mut rd = crate::room_data::test_minimal_room_data(owner);
        rd.self_sk = Some(SigningKey::from_bytes(&[sk_seed; 32]));
        rd
    }

    fn rooms_blob_with(owner: VerifyingKey, sk_seed: u8) -> Vec<u8> {
        let mut rooms = empty_rooms();
        rooms.map.insert(owner, room_with_identity(owner, sk_seed));
        cbor(&rooms)
    }

    /// A 3-generation lineage over a synthetic registry. Delegate key bytes
    /// `[10;32]`/`[11;32]`/`[12;32]` are generations 0/1/2.
    fn table3() -> [([u8; 32], [u8; 32]); 3] {
        [
            ([10u8; 32], [110u8; 32]),
            ([11u8; 32], [111u8; 32]),
            ([12u8; 32], [112u8; 32]),
        ]
    }

    fn lineage3() -> Vec<DelegateLineageEntry> {
        river_delegate_lineage(&table3())
    }

    fn pred_key(gen_index: usize) -> DelegateKey {
        let (k, ch) = table3()[gen_index];
        DelegateKey::new(k, CodeHash::new(ch))
    }

    // ---------------- in-memory seam ----------------

    /// In-memory [`RiverImportSeam`] holding a REAL `Rooms` + `MergeRanks`, so
    /// merges route through the production `Rooms::merge_from_source` — the
    /// #590/#527 machinery itself, not a reimplementation.
    struct MemorySeam {
        rooms: Rooms,
        ranks: MergeRanks,
        dm_merges: Vec<OutboundDmStore>,
        /// Archive entries, merged by the production hydration rule when the
        /// walk passes an archive rank.
        hidden: HashMap<
            (VerifyingKey, river_core::room_state::member::MemberId),
            river_core::chat_delegate::HiddenDmThreadEntry,
        >,
        flushes: usize,
        fail_flush: bool,
        /// Every rooms/DM merge reports this failure.
        fail_merge: Option<String>,
        /// The rail's order after every rooms merge: what the user would see
        /// render, one merge at a time (freenet/river#802).
        orders_seen: Vec<Vec<VerifyingKey>>,
        /// Lowerings and flushes, in call order.
        events: Vec<&'static str>,
        /// With `Some(n)`, order-only merges (no rooms, no tombstones) after
        /// the first `n` fail.
        order_only_merges_allowed: Option<usize>,
        /// Order-only merges refused under `order_only_merges_allowed`.
        order_only_refusals: usize,
        /// Order-only merges attempted (refused or not).
        order_only_merges: usize,
        /// `room_order` (unfiltered) after every successful rooms merge.
        raw_orders_seen: Vec<Vec<VerifyingKey>>,
        /// A merge carrying this room applies in full, then reports an error,
        /// like `merge_from_source` after a per-room failure (#591).
        err_after_apply_for: Option<VerifyingKey>,
    }

    impl MemorySeam {
        fn new() -> Self {
            Self {
                rooms: empty_rooms(),
                ranks: MergeRanks::default(),
                dm_merges: Vec::new(),
                hidden: HashMap::new(),
                flushes: 0,
                fail_flush: false,
                fail_merge: None,
                orders_seen: Vec::new(),
                events: Vec::new(),
                order_only_merges_allowed: None,
                order_only_refusals: 0,
                order_only_merges: 0,
                raw_orders_seen: Vec::new(),
                err_after_apply_for: None,
            }
        }
    }

    impl RiverImportSeam for MemorySeam {
        async fn merge_rooms(&mut self, rooms: Rooms, source_rank: u32) -> Result<(), String> {
            if let Some(e) = &self.fail_merge {
                return Err(e.clone());
            }
            if rooms.map.is_empty() && rooms.removed_rooms.is_empty() {
                self.order_only_merges += 1;
                if let Some(allowed) = self.order_only_merges_allowed.as_mut() {
                    if *allowed == 0 {
                        self.order_only_refusals += 1;
                        return Err("order-only merge refused".to_string());
                    }
                    *allowed -= 1;
                }
            }
            let partial = self
                .err_after_apply_for
                .is_some_and(|vk| rooms.map.contains_key(&vk));
            let merged =
                apply_recovered_rooms(&mut self.rooms, rooms, source_rank, &mut self.ranks);
            let merged = if partial {
                Err("another room in this merge failed".to_string())
            } else {
                merged
            };
            self.orders_seen.push(self.rooms.ordered_room_keys());
            self.raw_orders_seen.push(self.rooms.room_order.clone());
            merged
        }
        async fn merge_outbound_dms(
            &mut self,
            store: OutboundDmStore,
            archive_rank: Option<u32>,
        ) -> Result<(), String> {
            if let Some(e) = &self.fail_merge {
                return Err(e.clone());
            }
            if archive_rank.is_some() {
                let (to_insert, _) = chat_delegate::resolve_hidden_thread_hydration(
                    store.hidden_threads.clone(),
                    &self.hidden,
                    &HashSet::new(),
                );
                self.hidden.extend(to_insert);
            }
            self.dm_merges.push(store);
            Ok(())
        }
        async fn lower_identity_ranks(
            &mut self,
            provenance: Vec<(VerifyingKey, u32)>,
            from_rank: u32,
        ) {
            self.events.push("lower");
            crate::room_data::lower_identity_ranks(
                &mut self.ranks.identity,
                &provenance,
                from_rank,
            );
        }
        async fn known_rooms(&mut self) -> HashSet<[u8; 32]> {
            self.rooms
                .map
                .keys()
                .chain(self.rooms.removed_rooms.iter())
                .map(|vk| vk.to_bytes())
                .collect()
        }
        async fn flush(&mut self) -> Result<(), String> {
            self.events.push("flush");
            self.flushes += 1;
            if self.fail_flush {
                Err("coalesced save failed".to_string())
            } else {
                Ok(())
            }
        }
    }

    /// One journaled walk; the plain walk report.
    fn walk(channel: &MockChannel, seam: MemorySeam) -> (DelegateMigrationReport, MemorySeam) {
        let (journaled, seam) = walk_j(channel, seam);
        (journaled.report, seam)
    }

    /// One journaled walk, with the journal's status.
    fn walk_j(channel: &MockChannel, seam: MemorySeam) -> (JournaledMigrationReport, MemorySeam) {
        block_on(walk_with(channel, seam, successor_key(), &lineage3()))
    }

    // ---------------- marker keys ----------------

    /// Marker keys must hex-encode the predecessor key. The delegate's
    /// `create_origin_key` is `String::from_utf8_lossy`, which maps EVERY
    /// invalid byte to U+FFFD — so two predecessors whose raw keys differ only
    /// in invalid-UTF-8 bytes would alias onto ONE marker slot if the raw
    /// bytes were embedded. Hex is pure ASCII: the lossy pass is the identity.
    #[test]
    fn marker_keys_hex_encode_the_predecessor_and_survive_utf8_lossy() {
        let a = DelegateKey::new([0xFF; 32], CodeHash::new([1; 32]));
        let mut b_bytes = [0xFF; 32];
        b_bytes[0] = 0xFE; // still invalid UTF-8; lossy maps both to U+FFFD
        let b = DelegateKey::new(b_bytes, CodeHash::new([1; 32]));

        let ka = pred_done_marker_key(&a);
        let kb = pred_done_marker_key(&b);
        assert_ne!(ka, kb);
        // The delegate-side aliasing check: after the lossy pass the two
        // markers must STILL be distinct. Raw predecessor bytes fail this.
        assert_ne!(
            String::from_utf8_lossy(&ka),
            String::from_utf8_lossy(&kb),
            "marker keys must survive create_origin_key's utf8-lossy pass distinct"
        );
        assert!(ka.is_ascii(), "hex marker keys are pure ASCII");

        let expected: String = [0xFFu8; 32].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            ka,
            [MIGRATE_PRED_DONE_PREFIX, expected.as_bytes()].concat(),
            "done-marker key = prefix + lowercase hex of the predecessor key"
        );
        assert_eq!(
            pred_wip_marker_key(&a),
            [MIGRATE_PRED_WIP_PREFIX, expected.as_bytes()].concat(),
            "wip-marker key = wip prefix + same hex"
        );
        assert!(is_migration_marker_key(&ka));
        assert!(is_migration_marker_key(&pred_wip_marker_key(&a)));
        assert!(!is_migration_marker_key(ROOMS_STORAGE_KEY));
        assert!(!is_migration_marker_key(b"room:abc"));
    }

    // ---------------- candidate keys (the union rule) ----------------

    /// The fixed probes are asked even when the (corruptible) legacy index
    /// lists nothing; markers are excluded; `rooms_meta` goes first, so the
    /// walk merges the room order before the rooms (freenet/river#802).
    #[test]
    fn candidate_keys_union_fixed_probes_exclude_markers_meta_first() {
        // Empty index still probes the fixed keys, rooms_meta first.
        assert_eq!(
            candidate_keys(&[]),
            vec![
                ROOMS_META_KEY.to_vec(),
                ROOMS_STORAGE_KEY.to_vec(),
                OUTBOUND_DMS_STORAGE_KEY.to_vec(),
            ],
            "an empty legacy index must STILL probe rooms_data + outbound_dms + rooms_meta"
        );

        let listed = vec![
            ChatDelegateKey::new(b"room:abc".to_vec()),
            ChatDelegateKey::new(ROOMS_META_KEY.to_vec()),
            ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
            ChatDelegateKey::new(pred_done_marker_key(&pred_key(0))),
            ChatDelegateKey::new(pred_wip_marker_key(&pred_key(0))),
            ChatDelegateKey::new(b"room:abc".to_vec()), // duplicate
        ];
        let keys = candidate_keys(&listed);
        assert_eq!(
            keys,
            vec![
                ROOMS_META_KEY.to_vec(),
                b"room:abc".to_vec(),
                ROOMS_STORAGE_KEY.to_vec(),
                OUTBOUND_DMS_STORAGE_KEY.to_vec(),
            ],
            "dedup, no markers, fixed probes unioned, rooms_meta first"
        );
    }

    // ---------------- classification ----------------

    #[test]
    fn classify_recovered_maps_each_key_family() {
        let owner = vk(1);
        let blob = rooms_blob_with(owner, 2);
        assert!(matches!(
            classify_recovered(ROOMS_STORAGE_KEY, &blob),
            RecoveredItem::RoomsBlob(_)
        ));
        assert!(matches!(
            classify_recovered(ROOMS_STORAGE_KEY, b"not cbor at all"),
            RecoveredItem::Undecodable("rooms_data", _)
        ));
        let slot = cbor(&RoomSlot::Present(Box::new(room_with_identity(owner, 2))));
        let room_key = chat_delegate::room_storage_key(&owner);
        assert!(matches!(
            classify_recovered(&room_key, &slot),
            RecoveredItem::Room(k, RoomSlot::Present(_)) if k == owner
        ));
        let tomb = cbor(&RoomSlot::Tombstone);
        assert!(matches!(
            classify_recovered(&room_key, &tomb),
            RecoveredItem::Room(_, RoomSlot::Tombstone)
        ));
        assert!(matches!(
            classify_recovered(ROOMS_META_KEY, &cbor(&RoomsMeta::default())),
            RecoveredItem::Meta(_)
        ));
        assert!(matches!(
            classify_recovered(OUTBOUND_DMS_STORAGE_KEY, &cbor(&OutboundDmStore::default())),
            RecoveredItem::OutboundDms(_)
        ));
        assert!(matches!(
            classify_recovered(&pred_done_marker_key(&pred_key(0)), b"1"),
            RecoveredItem::MigrationMarker
        ));
        assert!(matches!(
            classify_recovered(b"some_future_key", b"whatever"),
            RecoveredItem::Unrecognized
        ));
    }

    /// A legacy blob's `current_room_key` is years-stale by construction and
    /// must never dictate selection (freenet/river#255) — stripped for the
    /// blob AND dropped from `rooms_meta`.
    #[test]
    fn rooms_to_merge_strips_the_legacy_cursor() {
        let owner = vk(1);
        let mut rooms = empty_rooms();
        rooms.map.insert(owner, room_with_identity(owner, 2));
        rooms.current_room_key = Some(owner);
        let item = RecoveredItem::RoomsBlob(Box::new(rooms));
        let merged = rooms_to_merge(&item).expect("blob converts");
        assert_eq!(
            merged.current_room_key, None,
            "legacy cursor must be stripped"
        );
        assert!(merged.map.contains_key(&owner));

        let meta = RecoveredItem::Meta(RoomsMeta {
            current_room_key: Some(owner),
            notification_modes: HashMap::new(),
            room_order: vec![owner],
            identity_sources: Vec::new(),
        });
        let merged = rooms_to_merge(&meta).expect("meta converts");
        assert_eq!(merged.current_room_key, None, "meta cursor must be dropped");
        assert_eq!(merged.room_order, vec![owner]);

        let tomb = RecoveredItem::Room(owner, RoomSlot::Tombstone);
        let merged = rooms_to_merge(&tomb).expect("tombstone converts");
        assert!(merged.removed_rooms.contains(&owner));
        assert!(merged.map.is_empty());
    }

    // ---------------- lineage ----------------

    /// The lineage is the registry, one entry per generation, in order — a
    /// mutation that DROPS or reorders a generation fails here.
    #[test]
    fn lineage_covers_every_registry_row_in_order() {
        let table = table3();
        let lineage = river_delegate_lineage(&table);
        assert_eq!(
            lineage.len(),
            table.len(),
            "every generation must be walked"
        );
        for (i, entry) in lineage.iter().enumerate() {
            assert_eq!(entry.generation, i as u32);
            assert_eq!(entry.delegate_key, table[i].0);
            assert_eq!(entry.code_hash, table[i].1);
        }
        // irregular_key is truthful: a derivable row reads false, a
        // non-derivable one (River's V1) reads true.
        let derived = *blake3::hash(&[7u8; 32]).as_bytes();
        let regular = river_delegate_lineage(&[([0u8; 32], [7u8; 32]), (derived, [7u8; 32])]);
        assert!(
            regular[0].irregular_key,
            "non-derivable key must be flagged"
        );
        assert!(
            !regular[1].irregular_key,
            "derivable key must not be flagged"
        );
    }

    /// THE RANK-SCALE AGREEMENT PIN: `RecoveredSecret::generation` is used
    /// directly as the merge's `source_rank`, which is only sound because the
    /// lineage generation and the sweep's `source_rank_for_delegate_key` are
    /// the SAME scale (the `LEGACY_DELEGATES` index). Run against the REAL
    /// registry: if either side reorders, this fails.
    #[test]
    fn lineage_generation_matches_the_sweep_merge_rank_scale() {
        let pairs = chat_delegate::legacy_delegate_pairs();
        assert!(
            pairs.len() >= 20,
            "the real registry has 20+ generations; got {} — wrong table?",
            pairs.len()
        );
        let lineage = river_delegate_lineage(pairs);
        for (entry, (dk, _)) in lineage.iter().zip(pairs.iter()) {
            assert_eq!(
                entry.generation,
                chat_delegate::source_rank_for_delegate_key(dk.as_slice()),
                "lineage generation and sweep merge rank diverged for a registry row"
            );
        }
    }

    // ---------------- scenarios through the real driver ----------------

    /// freenet/river#802 cause 1 through the real driver: the newest
    /// generation holds the full order but only some rooms; an older one holds
    /// the rest, which arrive later and must land in their place, at every
    /// step, not at the end.
    #[test]
    fn a_room_from_an_older_generation_lands_in_its_place() {
        let rooms = [vk(1), vk(2), vk(3)];
        let mut user_order = rooms.to_vec();
        user_order.sort_by(|a, b| b.as_bytes().cmp(a.as_bytes()));
        let slot = |room: VerifyingKey, seed: u8| {
            cbor(&RoomSlot::Present(Box::new(room_with_identity(room, seed))))
        };
        // gen1 (newer): the order, and every room but the first.
        let mut gen1 = DelegateSim::default();
        for (i, room) in user_order[1..].iter().enumerate() {
            gen1.store.insert(
                chat_delegate::room_storage_key(room),
                slot(*room, 30 + i as u8),
            );
        }
        gen1.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                room_order: user_order.clone(),
                ..Default::default()
            }),
        );
        // gen0 (older): the first room only, and no order of its own.
        let mut gen0 = DelegateSim::default();
        gen0.store.insert(
            chat_delegate::room_storage_key(&user_order[0]),
            slot(user_order[0], 40),
        );
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), gen1)
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), DelegateSim::default());

        let (report, seam) = walk(&channel, MemorySeam::new());

        assert!(report.is_complete(), "{report:?}");
        assert_eq!(seam.rooms.ordered_room_keys(), user_order);
        assert!(
            seam.orders_seen
                .iter()
                .any(|o| !o.is_empty() && !o.contains(&user_order[0])),
            "premise: the list rendered before the older generation's room arrived"
        );
        for seen in &seam.orders_seen {
            let expected: Vec<VerifyingKey> = user_order
                .iter()
                .filter(|k| seen.contains(k))
                .copied()
                .collect();
            assert_eq!(seen, &expected, "a partial list out of the user's order");
        }
    }

    /// freenet/river#802 review: `rooms_meta` merges before the rooms, so a
    /// room still tombstoned (by an older generation, at a lower rank) is
    /// skipped by that merge; this predecessor's `Present` slot then
    /// resurrects it. The order merged again at the flush must give it back
    /// its position, or it renders in the key-byte tail and the next save
    /// persists that.
    #[test]
    fn a_room_resurrected_by_the_walk_keeps_its_position() {
        let (r, a) = (vk(1), vk(2));
        // The user's order puts r after a; key-byte order alone would not.
        let (first, second) = if r.as_bytes() < a.as_bytes() {
            (a, r)
        } else {
            (r, a)
        };
        let mut seam = MemorySeam::new();
        // An older generation's tombstone for `first`, at rank 0.
        let mut older = empty_rooms();
        older.removed_rooms.insert(first);
        apply_recovered_rooms(&mut seam.rooms, older, 0, &mut seam.ranks).unwrap();
        assert!(seam.rooms.removed_rooms.contains(&first), "premise");

        let mut gen1 = DelegateSim::default();
        for (i, room) in [first, second].iter().enumerate() {
            gen1.store.insert(
                chat_delegate::room_storage_key(room),
                cbor(&RoomSlot::Present(Box::new(room_with_identity(
                    *room,
                    20 + i as u8,
                )))),
            );
        }
        gen1.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                room_order: vec![first, second],
                ..Default::default()
            }),
        );
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), gen1)
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), DelegateSim::default());

        let (report, seam) = walk(&channel, seam);

        assert!(report.is_complete(), "{report:?}");
        assert!(seam.rooms.map.contains_key(&first), "premise: resurrected");
        assert!(
            seam.raw_orders_seen.iter().any(|o| !o.contains(&first)),
            "premise: the early order merge skipped the tombstoned room"
        );
        assert_eq!(seam.rooms.ordered_room_keys(), vec![first, second]);
        assert_eq!(seam.rooms.room_order, vec![first, second]);
    }

    /// Two generations' orders disagree about a room an older slot
    /// resurrects: the flush re-merge places it by the NEWER order, in one
    /// folded merge per flush.
    #[test]
    fn the_newest_order_places_a_resurrected_room_in_one_merge() {
        let (r, a) = (vk(1), vk(2));
        let mut seam = MemorySeam::new();
        let mut oldest = empty_rooms();
        oldest.removed_rooms.insert(r);
        apply_recovered_rooms(&mut seam.rooms, oldest, 0, &mut seam.ranks).unwrap();
        let slot = |room: VerifyingKey, seed: u8| {
            cbor(&RoomSlot::Present(Box::new(room_with_identity(room, seed))))
        };
        let meta = |order: Vec<VerifyingKey>| {
            cbor(&RoomsMeta {
                room_order: order,
                ..Default::default()
            })
        };
        // gen2 (newest): r before a, and only a's slot.
        let mut gen2 = DelegateSim::default();
        gen2.store
            .insert(chat_delegate::room_storage_key(&a), slot(a, 32));
        gen2.store.insert(ROOMS_META_KEY.to_vec(), meta(vec![r, a]));
        // gen1 (older): a before r, and r's slot (resurrects it).
        let mut gen1 = DelegateSim::default();
        gen1.store
            .insert(chat_delegate::room_storage_key(&r), slot(r, 31));
        gen1.store.insert(ROOMS_META_KEY.to_vec(), meta(vec![a, r]));
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), gen1)
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());

        let (report, seam) = walk(&channel, seam);

        assert!(report.is_complete(), "{report:?}");
        assert!(seam.rooms.map.contains_key(&r), "premise: resurrected");
        assert!(
            seam.raw_orders_seen.iter().any(|o| !o.contains(&r)),
            "premise: the early order merge skipped the tombstoned room"
        );
        assert_eq!(seam.rooms.ordered_room_keys(), vec![r, a]);
        // gen2: its meta + one flush re-merge; gen1: its meta + ONE folded
        // re-merge of both orders.
        assert_eq!(seam.order_only_merges, 4);
    }

    /// A merge that resurrects a room and then reports a per-room error (it
    /// still merged the rest, #591) must still trigger the flush re-merge, or
    /// the resurrected room keeps the key-byte tail.
    #[test]
    fn a_partly_failed_merge_still_re_merges_the_order() {
        let (x, y) = (vk(1), vk(2));
        // `r` sorts after `a`, so the tail would put it last.
        let (a, r) = if x.as_bytes() < y.as_bytes() {
            (x, y)
        } else {
            (y, x)
        };
        let mut seam = MemorySeam::new();
        let mut oldest = empty_rooms();
        oldest.removed_rooms.insert(r);
        apply_recovered_rooms(&mut seam.rooms, oldest, 0, &mut seam.ranks).unwrap();
        seam.err_after_apply_for = Some(r);
        let slot = |room: VerifyingKey, seed: u8| {
            cbor(&RoomSlot::Present(Box::new(room_with_identity(room, seed))))
        };
        let mut gen2 = DelegateSim::default();
        gen2.store
            .insert(chat_delegate::room_storage_key(&a), slot(a, 42));
        gen2.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                room_order: vec![r, a],
                ..Default::default()
            }),
        );
        let mut gen1 = DelegateSim::default();
        gen1.store
            .insert(chat_delegate::room_storage_key(&r), slot(r, 41));
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), gen1)
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());

        let (_report, seam) = walk(&channel, seam);

        assert!(seam.rooms.map.contains_key(&r), "premise: resurrected");
        assert_eq!(seam.rooms.ordered_room_keys(), vec![r, a]);
    }

    /// A predecessor that merged no rooms (here, only a `rooms_meta`) does not
    /// re-merge at its flush: it cannot have resurrected anything.
    #[test]
    fn a_predecessor_without_rooms_does_not_re_merge() {
        let room = vk(1);
        let mut gen1 = DelegateSim::default();
        gen1.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                room_order: vec![room],
                ..Default::default()
            }),
        );
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), gen1)
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), DelegateSim::default());
        let (report, seam) = walk(&channel, MemorySeam::new());
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(seam.order_only_merges, 1, "the meta item only, no re-merge");
        assert_eq!(seam.rooms.room_order, vec![room]);
    }

    /// The same, when an OLDER predecessor's slot resurrects the room a newer
    /// predecessor's order placed (the newer one lacked its slot).
    #[test]
    fn a_room_resurrected_by_an_older_generation_keeps_its_position() {
        let (r, a) = (vk(1), vk(2));
        let (first, second) = if r.as_bytes() < a.as_bytes() {
            (a, r)
        } else {
            (r, a)
        };
        let mut seam = MemorySeam::new();
        let mut oldest = empty_rooms();
        oldest.removed_rooms.insert(first);
        apply_recovered_rooms(&mut seam.rooms, oldest, 0, &mut seam.ranks).unwrap();
        let slot = |room: VerifyingKey, seed: u8| {
            cbor(&RoomSlot::Present(Box::new(room_with_identity(room, seed))))
        };
        // gen2 (newest): the order and `second`, but no slot for `first`.
        let mut gen2 = DelegateSim::default();
        gen2.store
            .insert(chat_delegate::room_storage_key(&second), slot(second, 22));
        gen2.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                room_order: vec![first, second],
                ..Default::default()
            }),
        );
        // gen1 (older): `first`'s slot, no order of its own.
        let mut gen1 = DelegateSim::default();
        gen1.store
            .insert(chat_delegate::room_storage_key(&first), slot(first, 21));
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), gen1)
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());

        let (report, seam) = walk(&channel, seam);

        assert!(report.is_complete(), "{report:?}");
        assert!(seam.rooms.map.contains_key(&first), "premise: resurrected");
        assert!(
            seam.raw_orders_seen.iter().any(|o| !o.contains(&first)),
            "premise: the early order merge skipped the tombstoned room"
        );
        assert_eq!(seam.rooms.ordered_room_keys(), vec![first, second]);
    }

    /// The flush-time order re-merge only restores positions: its failure
    /// must not fail the predecessor (its rooms merged and must be sealed).
    #[test]
    fn a_failed_order_re_merge_does_not_fail_the_predecessor() {
        let room = vk(1);
        let mut gen1 = DelegateSim::default();
        gen1.store.insert(
            chat_delegate::room_storage_key(&room),
            cbor(&RoomSlot::Present(Box::new(room_with_identity(room, 21)))),
        );
        // An identity source naming gen0 (the registry row of pred_key(0)),
        // so the flush has a lowering to do after the refused re-merge.
        gen1.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                room_order: vec![room],
                identity_sources: vec![(room, *pred_key(0).bytes().first_chunk::<32>().unwrap())],
                ..Default::default()
            }),
        );
        let mut seam = MemorySeam::new();
        // The `rooms_meta` item's own merge goes through; the flush-time
        // re-merge of its order is refused.
        seam.order_only_merges_allowed = Some(1);
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), gen1)
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), DelegateSim::default());
        let (report, seam) = walk(&channel, seam);
        assert_eq!(
            seam.order_only_refusals, 1,
            "premise: the flush-time re-merge ran (once: only gen1 merged rooms) \
             and was refused"
        );
        assert!(report.is_complete(), "{report:?}");
        assert!(seam.rooms.map.contains_key(&room));
        // The refused re-merge did not skip the lowering or the flush.
        let lower = seam.events.iter().position(|e| *e == "lower");
        assert!(lower.is_some(), "the lowering still ran: {:?}", seam.events);
        assert_eq!(seam.events.get(lower.unwrap() + 1), Some(&"flush"));
    }

    /// freenet/river#802 cause 2: the walk merges a predecessor's rooms one at
    /// a time, and each merge renders. The rail must show the user's order at
    /// every step, not key-byte order until `rooms_meta` lands and then jump.
    #[test]
    fn walk_shows_the_users_room_order_at_every_merge() {
        let rooms = [vk(1), vk(2), vk(3)];
        // The user's order, deliberately the reverse of key-byte order.
        let mut user_order = rooms.to_vec();
        user_order.sort_by(|a, b| b.as_bytes().cmp(a.as_bytes()));
        let mut gen0 = DelegateSim::default();
        for (i, room) in rooms.iter().enumerate() {
            gen0.store.insert(
                chat_delegate::room_storage_key(room),
                cbor(&RoomSlot::Present(Box::new(room_with_identity(
                    *room,
                    10 + i as u8,
                )))),
            );
        }
        gen0.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                room_order: user_order.clone(),
                ..Default::default()
            }),
        );
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), DelegateSim::default());

        let (report, seam) = walk(&channel, MemorySeam::new());

        assert!(report.is_complete(), "{report:?}");
        assert_eq!(seam.rooms.ordered_room_keys(), user_order);
        assert!(
            seam.orders_seen.iter().any(|o| o.len() > 1),
            "premise: several rooms rendered before the walk finished"
        );
        for seen in &seam.orders_seen {
            let expected: Vec<VerifyingKey> = user_order
                .iter()
                .filter(|k| seen.contains(k))
                .copied()
                .collect();
            assert_eq!(seen, &expected, "a partial list out of the user's order");
        }
    }

    /// The recovery walk end-to-end: a single-blob-era generation, a
    /// per-room-era generation, and an empty newest generation. Everything is
    /// recovered, every predecessor is sealed with a DURABLE marker on the
    /// successor, and the marker value distinguishes data from empty.
    #[test]
    fn full_walk_recovers_all_generations_and_seals_durable_markers() {
        let room_a = vk(1);
        let room_b = vk(2);
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(room_a, 3));
        gen0.store.insert(
            OUTBOUND_DMS_STORAGE_KEY.to_vec(),
            cbor(&OutboundDmStore::default()),
        );
        let mut gen1 = DelegateSim::default();
        gen1.store.insert(
            chat_delegate::room_storage_key(&room_b),
            cbor(&RoomSlot::Present(Box::new(room_with_identity(room_b, 4)))),
        );
        gen1.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                current_room_key: Some(room_b),
                notification_modes: HashMap::new(),
                room_order: vec![room_b],
                identity_sources: Vec::new(),
            }),
        );
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), gen1)
            .with_delegate(&pred_key(2), DelegateSim::default()) // executes, no data
            .with_delegate(&successor_key(), DelegateSim::default());

        let (report, seam) = walk(&channel, MemorySeam::new());

        assert!(
            report.is_complete(),
            "clean walk must be complete: {report:?}"
        );
        assert!(!report.any_unresponsive());
        assert!(
            seam.rooms.map.contains_key(&room_a),
            "single-blob era recovered"
        );
        assert!(
            seam.rooms.map.contains_key(&room_b),
            "per-room era recovered"
        );
        assert_eq!(
            seam.rooms.current_room_key, None,
            "legacy cursor never adopted"
        );
        assert_eq!(
            seam.rooms.room_order,
            vec![room_b],
            "meta view prefs recovered"
        );
        assert_eq!(seam.dm_merges.len(), 1, "outbound-DM store recovered");
        assert!(seam.flushes >= 2, "each data-bearing predecessor flushes");

        // Durable markers on the SUCCESSOR, value distinguishing data/empty.
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(0))),
            Some(PRED_DONE_MARKER_VALUE_DATA.to_vec())
        );
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(1))),
            Some(PRED_DONE_MARKER_VALUE_DATA.to_vec())
        );
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(2))),
            Some(PRED_DONE_MARKER_VALUE_EMPTY.to_vec()),
            "an executed-but-empty predecessor seals with the EMPTY value"
        );
    }

    /// The durable-marker payoff: a second page load's walk short-circuits on
    /// the sealed markers — no predecessor is re-fetched (this is what fixes
    /// the gateway-iframe re-probe waste, where localStorage cannot persist
    /// the sweep's own done flag).
    #[test]
    fn a_second_walk_short_circuits_on_the_durable_markers() {
        let room_a = vk(1);
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(room_a, 3));
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), DelegateSim::default());

        let (first, _) = walk(&channel, MemorySeam::new());
        assert!(first.is_complete());

        channel.reset_counters();
        let (second, seam) = walk(&channel, MemorySeam::new());
        assert!(
            second
                .predecessors
                .iter()
                .all(|p| matches!(p, PredecessorMigration::AlreadyMigrated { .. })),
            "sealed predecessors must short-circuit: {second:?}"
        );
        assert_eq!(
            channel.requests_to_predecessors("get:"),
            0,
            "a sealed predecessor must never be re-fetched"
        );
        assert!(
            seam.rooms.map.is_empty(),
            "nothing re-imported on a sealed walk"
        );
    }

    /// **Timeout is not absence** (the freenet-migrate#19 override). A
    /// predecessor that proves executable but goes silent on a GET must be
    /// `Unresponsive` — never `NoData`, never sealed. Collapsing the silent
    /// GET into "absent" seals the predecessor EMPTY and strands its data
    /// forever; that mutation turns all three assertions red.
    #[test]
    fn a_silent_get_is_unresponsive_never_absent_never_sealed() {
        let room_a = vk(1);
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(room_a, 3));
        gen0.silent_get_keys = vec![ROOMS_STORAGE_KEY.to_vec()];
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), DelegateSim::default());

        let (report, seam) = walk(&channel, MemorySeam::new());

        assert!(
            report.predecessors.iter().any(|p| matches!(
                p,
                PredecessorMigration::Unresponsive {
                    generation: 0,
                    error: Some(_),
                    ..
                }
            )),
            "a silent GET after a positive probe is a FAULT: {report:?}"
        );
        assert!(!report.is_complete());
        assert!(report.retry_may_help(), "the walk must retry next load");
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(0))),
            None,
            "a predecessor that went silent must NOT be sealed"
        );
        assert!(seam.rooms.map.is_empty());
    }

    /// A predecessor whose probe gets silence (never installed, or broken) is
    /// `Unresponsive` — and under River's Union policy the walk CONTINUES to
    /// older generations instead of stranding them (freenet/river#204).
    /// Switching the policy to `NewestSnapshotWins` supersedes gen 0 and
    /// turns this red.
    #[test]
    fn a_silent_predecessor_does_not_halt_the_union_walk() {
        let room_a = vk(1);
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(room_a, 3));
        let mut gen2 = DelegateSim::default();
        gen2.silent = true;
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());

        let (report, seam) = walk(&channel, MemorySeam::new());

        assert!(
            report.predecessors.iter().any(|p| matches!(
                p,
                PredecessorMigration::Unresponsive {
                    generation: 2,
                    error: None,
                    ..
                }
            )),
            "clean silence is Unresponsive with error: None: {report:?}"
        );
        assert!(
            seam.rooms.map.contains_key(&room_a),
            "Union must keep walking past a silent newer generation (river#204)"
        );
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(2))),
            None,
            "the silent generation is NOT sealed — a later load retries it"
        );
        assert!(!report.is_complete());
    }

    /// The predecessor gen0 is `Unresponsive` with an error, NOT sealed, and
    /// nothing of it was imported: the shape every "short export" fault must
    /// take (freenet/river#592, #763).
    fn assert_gen0_refused(
        report: &DelegateMigrationReport,
        channel: &MockChannel,
        seam: &MemorySeam,
    ) {
        assert!(
            report.predecessors.iter().any(|p| matches!(
                p,
                PredecessorMigration::Unresponsive {
                    generation: 0,
                    error: Some(_),
                    ..
                }
            )),
            "a short export must be a fault, never data: {report:?}"
        );
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(0))),
            None,
            "a refused predecessor must not be sealed"
        );
        assert!(seam.rooms.map.is_empty(), "nothing of it may be imported");
        assert!(!report.is_complete());
    }

    /// A short-index refusal (#768 round 3, S3): gen 0 stays pending and
    /// unsealed, the rooms its `rooms_meta` proves exist (`salvaged`) are
    /// recovered with their identity, and nothing else of it (`not_imported`)
    /// is imported by the walk.
    fn assert_gen0_refused_with_salvage(
        report: &DelegateMigrationReport,
        channel: &MockChannel,
        seam: &MemorySeam,
        salvaged: &[VerifyingKey],
        not_imported: &[VerifyingKey],
    ) {
        assert!(
            report.predecessors.iter().any(|p| matches!(
                p,
                PredecessorMigration::Unresponsive {
                    generation: 0,
                    error: Some(_),
                    ..
                }
            )),
            "a short export must be a fault, never data: {report:?}"
        );
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(0))),
            None,
            "a refused predecessor must not be sealed"
        );
        assert!(!report.is_complete());
        for vk in salvaged {
            let room = seam.rooms.map.get(vk).expect("a proven room is recovered");
            assert!(room.self_sk.is_some(), "with its signing key");
        }
        for vk in not_imported {
            assert!(
                !seam.rooms.map.contains_key(vk),
                "the walk imports nothing else"
            );
        }
    }

    /// #768 round 3, S2 + S3: a short index is proven from `rooms_meta` FIRST,
    /// without downloading the listed rooms; the proven room is recovered once;
    /// and a later walk over the unchanged predecessor refuses it without
    /// downloading any room slot, while a changed one is checked again.
    #[test]
    fn a_short_index_is_refused_cheaply_and_its_proven_rooms_recovered_once() {
        let (a, b) = (vk(1), vk(2));
        let slot = |r, seed| cbor(&RoomSlot::Present(Box::new(room_with_identity(r, seed))));
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(chat_delegate::room_storage_key(&a), slot(a, 3));
        gen0.store
            .insert(chat_delegate::room_storage_key(&b), slot(b, 4));
        gen0.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                room_order: vec![b, a],
                ..Default::default()
            }),
        );
        gen0.listed = Some(vec![
            chat_delegate::room_storage_key(&b),
            ROOMS_META_KEY.to_vec(),
        ]);
        let channel = channel_with_gen0(gen0);
        let b_slot = format!(
            "get:{}",
            String::from_utf8_lossy(&chat_delegate::room_storage_key(&b))
        );
        let a_slot = format!(
            "get:{}",
            String::from_utf8_lossy(&chat_delegate::room_storage_key(&a))
        );

        let (report, seam) = walk(&channel, MemorySeam::new());
        assert_gen0_refused_with_salvage(&report, &channel, &seam, &[a], &[b]);
        assert_eq!(
            channel.requests_to_predecessors(&b_slot),
            0,
            "listed rooms are not downloaded to prove it"
        );
        assert_eq!(channel.requests_to_predecessors(&a_slot), 1);
        let record = block_on(read_short_index_record(&channel, &successor_key())).unwrap();
        assert_eq!(
            record.len(),
            1,
            "the refusal is recorded once its rooms flushed"
        );
        assert_eq!(record[0].0, pred_key(0).bytes()[..]);

        // Unchanged: refused again without any room slot downloaded.
        channel.reset_counters();
        assert!(seam.flushes > 0);
        let (report, seam2) = walk(&channel, MemorySeam::new());
        assert!(!report.is_complete());
        assert_eq!(
            channel.requests_to_predecessors(&a_slot),
            0,
            "{:?}",
            channel.log.borrow()
        );
        assert_eq!(channel.requests_to_predecessors(&b_slot), 0);
        assert!(seam2.rooms.map.is_empty(), "nothing re-merged");

        // Changed (an old tab wrote): checked again.
        channel.put(
            &pred_key(0),
            ROOMS_META_KEY,
            &cbor(&RoomsMeta {
                room_order: vec![a, b],
                ..Default::default()
            }),
        );
        channel.reset_counters();
        let (_, seam3) = walk(&channel, MemorySeam::new());
        assert_eq!(channel.requests_to_predecessors(&a_slot), 1);
        assert!(seam3.rooms.map.contains_key(&a));
    }

    /// S2 + S3: a salvage that did not land (the flush failed) records no
    /// refusal, so the next walk proves and salvages again.
    #[test]
    fn a_salvage_that_did_not_flush_is_not_recorded() {
        let (a, b) = (vk(1), vk(2));
        let slot = |r, seed| cbor(&RoomSlot::Present(Box::new(room_with_identity(r, seed))));
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(chat_delegate::room_storage_key(&a), slot(a, 3));
        gen0.store
            .insert(chat_delegate::room_storage_key(&b), slot(b, 4));
        gen0.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                room_order: vec![b, a],
                ..Default::default()
            }),
        );
        gen0.listed = Some(vec![
            chat_delegate::room_storage_key(&b),
            ROOMS_META_KEY.to_vec(),
        ]);
        let channel = channel_with_gen0(gen0);
        let mut seam = MemorySeam::new();
        seam.fail_flush = true;
        let _ = walk(&channel, seam);
        let record = block_on(read_short_index_record(&channel, &successor_key())).unwrap();
        assert!(
            record.is_empty(),
            "a salvage that did not land must not be recorded"
        );
    }

    fn gen0_with_blob() -> DelegateSim {
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(vk(1), 3));
        gen0
    }

    fn channel_with_gen0(gen0: DelegateSim) -> MockChannel {
        MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), DelegateSim::default())
    }

    /// freenet/river#592: the frozen legacy WASM turns an index it cannot
    /// decode into an EMPTY listing. The raw index read sees the undecodable
    /// bytes, and the export is refused rather than certified as the fixed
    /// keys alone (which would strand every per-room key for good once the
    /// journal believes it). The sweep still reads the fixed keys itself.
    #[test]
    fn a_corrupt_index_is_a_fault_not_a_short_export() {
        let mut gen0 = gen0_with_blob();
        gen0.listed = Some(vec![]); // the frozen WASM's answer
        gen0.raw_index = Some(Some(vec![0xFF, 0x00, 0x13])); // what is stored
        let channel = channel_with_gen0(gen0);
        let (report, seam) = walk(&channel, MemorySeam::new());
        assert_gen0_refused(&report, &channel, &seam);
    }

    /// Every registered generation indexes what it stores, so an ABSENT index
    /// beside a stored fixed key is a read fault (freenet-core#5857 reports a
    /// failed read as "not found"), not an empty delegate.
    #[test]
    fn an_absent_index_beside_a_stored_fixed_key_is_a_fault() {
        let mut gen0 = gen0_with_blob();
        gen0.listed = Some(vec![]);
        gen0.raw_index = Some(None);
        let channel = channel_with_gen0(gen0);
        let (report, seam) = walk(&channel, MemorySeam::new());
        assert_gen0_refused(&report, &channel, &seam);
    }

    /// A frozen predecessor does not change, so two reads of its index that
    /// disagree mean one of them failed.
    #[test]
    fn two_disagreeing_index_reads_are_a_fault() {
        let mut gen0 = gen0_with_blob();
        gen0.raw_index_reread = Some(None);
        let channel = channel_with_gen0(gen0);
        let (report, seam) = walk(&channel, MemorySeam::new());
        assert_gen0_refused(&report, &channel, &seam);
    }

    /// The predecessor builds its listing from the same stored index, so a
    /// listing that disagrees with it is a read fault on one side.
    #[test]
    fn a_listing_that_disagrees_with_the_stored_index_is_a_fault() {
        let room_b = vk(2);
        let mut gen0 = gen0_with_blob();
        gen0.listed = Some(vec![ROOMS_STORAGE_KEY.to_vec()]);
        let index = RawKeyIndex {
            keys: vec![
                ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
                ChatDelegateKey::new(chat_delegate::room_storage_key(&room_b)),
            ],
        };
        gen0.raw_index = Some(Some(cbor(&index)));
        let channel = channel_with_gen0(gen0);
        let (report, seam) = walk(&channel, MemorySeam::new());
        assert_gen0_refused(&report, &channel, &seam);
    }

    /// A listed key answering "no value" is a read fault, never a skip
    /// (freenet/river#763's decision on a stale index entry): River has never
    /// sent a `DeleteRequest` in production, and the re-keyed delegate updates
    /// the index before removing a value, so no deployed or future generation
    /// can hold a stale entry. Skipping it would let a failed read of one room
    /// certify the rest as everything the predecessor held.
    #[test]
    fn a_listed_key_answering_no_value_is_a_fault_not_a_skip() {
        let room_b = vk(2);
        let mut gen0 = gen0_with_blob();
        gen0.listed = Some(vec![
            ROOMS_STORAGE_KEY.to_vec(),
            chat_delegate::room_storage_key(&room_b), // listed, value unreadable
        ]);
        let channel = channel_with_gen0(gen0);
        let (report, seam) = walk(&channel, MemorySeam::new());
        assert_gen0_refused(&report, &channel, &seam);
    }

    /// The happy path still recovers through the loud fetch: listing, raw
    /// index (twice) and values all agree.
    #[test]
    fn a_consistent_predecessor_still_imports_through_the_loud_fetch() {
        let channel = channel_with_gen0(gen0_with_blob());
        let (report, seam) = walk(&channel, MemorySeam::new());
        assert!(report.is_complete(), "{report:?}");
        assert!(seam.rooms.map.contains_key(&vk(1)));
        assert_eq!(
            channel.requests_to_predecessors(&format!(
                "get:{}",
                String::from_utf8_lossy(KEY_INDEX_STORAGE_KEY)
            )),
            6,
            "the raw index is read exactly twice per executable predecessor (3 here)"
        );
    }

    /// A GET reply naming a DIFFERENT key is a mis-correlated answer; acting
    /// on it would attribute one key's bytes to another. Removing the
    /// key-mismatch check imports the smuggled room and turns this red.
    #[test]
    fn a_mismatched_get_reply_is_a_fault_not_data() {
        let room_z = vk(9);
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(vk(1), 3));
        // Asking for rooms_data yields a reply that names outbound_dms but
        // carries a decodable rooms blob with room Z.
        gen0.wrong_key_reply = Some((
            ROOMS_STORAGE_KEY.to_vec(),
            OUTBOUND_DMS_STORAGE_KEY.to_vec(),
            rooms_blob_with(room_z, 5),
        ));
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), DelegateSim::default());

        let (report, seam) = walk(&channel, MemorySeam::new());

        assert!(
            !seam.rooms.map.contains_key(&room_z),
            "a mis-correlated reply's bytes must never be imported"
        );
        assert!(
            report.predecessors.iter().any(|p| matches!(
                p,
                PredecessorMigration::Unresponsive {
                    generation: 0,
                    error: Some(_),
                    ..
                }
            )),
            "the mismatch is a fault, not data: {report:?}"
        );
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(0))),
            None
        );
    }

    /// An unreadable marker MUST stop the walk (`WriterUnavailable`) —
    /// guessing "no marker" would re-import a possibly-done predecessor, and
    /// guessing "done" would silently skip one. Mutating `get_marker`'s
    /// timeout arm to `Ok(None)` imports everything (rooms non-empty → red);
    /// mutating it to a fabricated `Done` completes the report (→ red).
    #[test]
    fn an_unreadable_marker_stops_the_walk_before_any_import() {
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(vk(1), 3));
        let mut successor = DelegateSim::default();
        successor.silent = true; // marker reads time out
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), successor);

        let (report, seam) = walk(&channel, MemorySeam::new());

        assert!(
            matches!(
                report.predecessors.first(),
                Some(PredecessorMigration::WriterUnavailable { .. })
            ),
            "an unreadable marker is WriterUnavailable, never None/Done: {report:?}"
        );
        assert!(seam.rooms.map.is_empty(), "nothing may be imported blind");
        assert_eq!(seam.flushes, 0);
        assert!(!report.is_complete());
        assert!(report.retry_may_help());
        assert_eq!(
            channel.requests_to_predecessors("get:"),
            0,
            "no predecessor fetch may run without marker bookkeeping"
        );
    }

    /// A failed flush (the coalesced save reporting failure) withholds the
    /// completion marker, so the next load re-walks the predecessor. No-op'ing
    /// `flush_predecessor` while the save fails seals the predecessor with the
    /// data unpersisted — every assertion here goes red under that mutation.
    #[test]
    fn a_failed_flush_withholds_the_completion_marker() {
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(vk(1), 3));
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), DelegateSim::default());
        let mut seam = MemorySeam::new();
        seam.fail_flush = true;

        let (report, seam) = walk(&channel, seam);

        assert!(seam.flushes >= 1, "the flush must actually be attempted");
        assert!(
            report.predecessors.iter().any(|p| matches!(
                p,
                PredecessorMigration::Incomplete {
                    generation: 0,
                    failure: Some(WriterFailure {
                        stage: WriterStage::Flush,
                        ..
                    }),
                    ..
                }
            )),
            "a failed flush is an Incomplete predecessor: {report:?}"
        );
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(0))),
            None,
            "the completion marker must be withheld when the save failed"
        );
        // The in-progress marker IS durable, so the interrupted run is
        // re-walked next load.
        assert!(
            channel
                .stored(&successor_key(), &pred_wip_marker_key(&pred_key(0)))
                .is_some(),
            "the in-progress marker must be recorded before items"
        );
        assert!(report.retry_may_help());
    }

    // ---------------- ranked merge (#590 / #527), through the walk ----------

    /// #527: a room already held with a HIGHER-ranked identity keeps it when a
    /// legacy generation offers a conflicting one; a room held from an OLDER
    /// source adopts the identity a NEWER generation offers. Both directions
    /// prove `RecoveredSecret::generation` is plumbed through as the merge's
    /// source rank — a raw-slot-write mutation (or a constant rank) flips one
    /// direction and turns this red.
    #[test]
    fn identity_conflicts_resolve_by_generation_rank_not_arrival() {
        let room_x = vk(1);
        let room_y = vk(2);
        // gen2 (rank 2) offers X with identity 40 and Y with identity 41.
        let mut gen2 = DelegateSim::default();
        let mut blob = empty_rooms();
        blob.map.insert(room_x, room_with_identity(room_x, 40));
        blob.map.insert(room_y, room_with_identity(room_y, 41));
        gen2.store.insert(ROOMS_STORAGE_KEY.to_vec(), cbor(&blob));
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());

        // The successor already holds X (identity 50, rank 5 — newer than any
        // generation) and Y (identity 51, rank 0 — an older-era load).
        let mut seam = MemorySeam::new();
        seam.rooms
            .map
            .insert(room_x, room_with_identity(room_x, 50));
        seam.ranks.identity.insert(room_x.to_bytes(), 5);
        seam.rooms
            .map
            .insert(room_y, room_with_identity(room_y, 51));
        seam.ranks.identity.insert(room_y.to_bytes(), 0);

        let (_report, seam) = walk(&channel, seam);

        let x_identity = seam.rooms.map[&room_x].self_verifying_key().unwrap();
        assert_eq!(
            x_identity,
            SigningKey::from_bytes(&[50; 32]).verifying_key(),
            "a higher-ranked identity must survive a legacy offer (#527)"
        );
        let y_identity = seam.rooms.map[&room_y].self_verifying_key().unwrap();
        assert_eq!(
            y_identity,
            SigningKey::from_bytes(&[41; 32]).verifying_key(),
            "a strictly newer generation must correct an older-ranked identity (#527)"
        );
    }

    /// #590: tombstones are RANKED observations. An older generation's
    /// tombstone must not delete a room held from a newer source, and a newer
    /// generation's `Present` must resurrect a room tombstoned by an older
    /// one. A merge→overwrite mutation flattens both and turns this red.
    #[test]
    fn tombstones_are_ranked_observations_through_the_walk() {
        let room_kept = vk(1);
        let room_back = vk(2);
        // gen2 (rank 2, walked FIRST) holds both rooms present.
        let mut gen2 = DelegateSim::default();
        let mut newer = empty_rooms();
        newer
            .map
            .insert(room_kept, room_with_identity(room_kept, 30));
        newer
            .map
            .insert(room_back, room_with_identity(room_back, 31));
        gen2.store.insert(ROOMS_STORAGE_KEY.to_vec(), cbor(&newer));
        // gen0 (rank 0, walked LAST) tombstones room_kept.
        let mut gen0 = DelegateSim::default();
        let mut older = empty_rooms();
        older.removed_rooms.insert(room_kept);
        gen0.store.insert(ROOMS_STORAGE_KEY.to_vec(), cbor(&older));
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());

        // The successor carries a rank-0 tombstone for room_back from an
        // older-era load this session.
        let mut seam = MemorySeam::new();
        seam.rooms.removed_rooms.insert(room_back);
        seam.ranks.tombstone.insert(room_back.to_bytes(), 0);

        let (_report, seam) = walk(&channel, seam);

        assert!(
            seam.rooms.map.contains_key(&room_kept),
            "an OLDER generation's tombstone must not delete a newer Present (#590)"
        );
        assert!(
            !seam.rooms.removed_rooms.contains(&room_kept),
            "the stale tombstone must not be recorded over the live room"
        );
        assert!(
            seam.rooms.map.contains_key(&room_back),
            "a NEWER generation's Present must resurrect an older tombstone (#590)"
        );
        assert!(
            seam.ranks.resurrected.contains(&room_back.to_bytes()),
            "the resurrection must be recorded for the save path (mark_room_rejoined)"
        );
    }

    /// **`LiveImportSeam::merge_outbound_dms` must not return until its writes
    /// have landed.**
    ///
    /// This pin is a source scrape rather than a behavioural test, and that is
    /// deliberate: nothing below it can see the bug. `util::defer` is
    /// `setTimeout(0)` on wasm32 but a SYNCHRONOUS call natively (`util.rs`),
    /// and `MemorySeam` merges synchronously too — so on every surface a test
    /// can run, the deferred write has always landed by the time `flush` reads.
    /// The ordering only exists in the browser. A behavioural test here would
    /// pass identically with the bug present, which is worse than no test.
    ///
    /// The bug: the PUBLIC hydrate helpers defer their signal writes as a
    /// macrotask and return immediately, while `flush` reads `OUTBOUND_DMS`
    /// synchronously on a microtask chain that drains first. For a DM-only
    /// predecessor the save serialised the PRE-merge cache, the driver sealed
    /// `Done { had_data: true }`, and the marker guaranteed the predecessor was
    /// never re-fetched — silent, permanent DM loss, worst in the gateway
    /// iframe where the marker is the only authority.
    #[test]
    fn merge_outbound_dms_awaits_its_deferred_writes() {
        let src = include_str!("delegate_migration.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");
        let body = production
            .split("async fn merge_outbound_dms(")
            .nth(1)
            .expect("LiveImportSeam::merge_outbound_dms must exist")
            .split("\n    async fn ")
            .next()
            .expect("merge_outbound_dms body");

        assert!(
            body.contains("rx.await"),
            "merge_outbound_dms must AWAIT its deferred writes before returning, \
             as merge_rooms does — otherwise flush snapshots the pre-merge cache \
             and the driver seals Done over DM data that was never saved"
        );
        // It must use the non-deferring cores inside its own single defer. The
        // public helpers would re-defer, putting the writes back on a later
        // macrotask and restoring the bug even with the await in place.
        for helper in [
            "hydrate_hidden_dm_threads_now(",
            "hydrate_outbound_dms_cache_now(",
        ] {
            assert!(
                body.contains(helper),
                "merge_outbound_dms must call {helper} inside its own defer"
            );
        }
        assert!(
            !body.contains("hydrate_hidden_dm_threads(")
                && !body.contains("hydrate_outbound_dms_cache("),
            "merge_outbound_dms must NOT call the deferring public hydrate \
             helpers — they schedule a further macrotask, so the awaited signal \
             fires before the writes land"
        );
        // #768 round 4, M1: the archive import runs only for a caller that
        // passes a rank (the top-up passes `None`).
        let guard = body
            .find("if archive_rank.is_some() {")
            .expect("the archive import is guarded by archive_rank");
        let import = body.find("hydrate_hidden_dm_threads_now(").expect("import");
        assert!(guard < import && import < guard + 200);
        // The load-bearing ordering: the completion signal must be sent from
        // INSIDE the defer closure, after the writes. Signalling before the
        // defer satisfies every assertion above while restoring the bug
        // exactly, because the await then returns before the writes land.
        let defer_at = body
            .find("defer(move ||")
            .expect("the writes must run inside a defer closure");
        // The SUCCESS signal: an early `tx.send(Err(..))` for a superseded
        // attempt (#768 round 2 item 5) writes nothing and returns.
        let send_at = body
            .find("tx.send(Ok(")
            .expect("the oneshot must be signalled");
        // Bound by the closure's TERMINATOR, not just its opening. `defer_at <
        // send_at` alone passes for a `tx.send` placed AFTER the whole closure
        // — which compiles (tx simply is not captured), satisfies every other
        // assertion here, and restores B2 exactly, because the await then
        // returns before the macrotask runs.
        // Brace-matched from the `defer(` opening rather than `find("});")`.
        // A first-match anchor breaks on any nested closure call inside the
        // defer: `merge_rooms` right next door already has that shape
        // (`ROOMS.with_mut(|current| { ... });` closes long before its
        // `tx.send`), so the same pin applied there would fail on correct code
        // while claiming B2 had been restored — the misdescribing failure this
        // file has been bitten by before.
        let closure_end = body[defer_at..]
            .char_indices()
            .scan(0i32, |depth, (i, c)| {
                match c {
                    '{' => *depth += 1,
                    '}' => *depth -= 1,
                    _ => {}
                }
                Some((i, *depth))
            })
            .skip_while(|(_, d)| *d == 0)
            .find(|(_, d)| *d == 0)
            .map(|(i, _)| i)
            .map(|off| defer_at + off)
            .expect("the defer closure must be terminated");
        assert!(
            defer_at < send_at && send_at < closure_end,
            "tx.send must happen INSIDE the defer closure, after the writes — \
             signalling outside it makes the await return pre-merge (B2)"
        );
        for helper in [
            "hydrate_hidden_dm_threads_now(",
            "hydrate_outbound_dms_cache_now(",
        ] {
            let at = body.find(helper).expect("helper call");
            assert!(
                at > defer_at && at < send_at,
                "{helper} must run inside the defer, BEFORE the signal"
            );
        }
    }

    /// The PRODUCTION `request_all` must keep the safe shape.
    ///
    /// `MockChannel` deliberately fans out naively, so every `max_in_flight`
    /// assertion measures the double rather than production. Nothing else
    /// exercises `NodeDelegateChannel::request_all` — so without this, deleting
    /// its override or rewriting it as a `join_all` over `request` would return
    /// production to the overlapping-`WEB_API`-borrow shape with all tests green.
    #[test]
    fn production_request_all_enqueues_before_it_awaits() {
        let src = include_str!("delegate_migration.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");
        let body = production
            .split("impl RiverDelegateChannel for NodeDelegateChannel {")
            .nth(1)
            .expect("the production channel impl must exist")
            .split("\n}")
            .next()
            .expect("impl body");
        let request_all = body.split("async fn request_all(").nth(1).expect(
            "NodeDelegateChannel must define request_all — the trait has \
                     no default, but a re-added default would make this silent",
        );

        let last_enqueue = request_all
            .rfind("enqueue_delegate_request")
            .expect("request_all must enqueue");
        let first_await = request_all
            .find("await_delegate_response_outcome")
            .expect("request_all must await the replies");
        assert!(
            last_enqueue < first_await,
            "every send must be enqueued BEFORE the first reply is awaited, so \
             the WEB_API borrows never overlap and all requests are still in \
             flight before any reply is awaited"
        );
        assert!(
            request_all[..last_enqueue].contains("for "),
            "the enqueue phase must be a sequential loop, not a concurrent \
             combinator — concurrency here is the borrow hazard itself"
        );
        // Text order alone is ALSO satisfied by a fully sequential rewrite that
        // enqueues and awaits inside one loop. That stays borrow-safe but
        // destroys the concurrency prewarm exists for: 27 predecessors would
        // serialise into 27 x the 10 s timeout, the exact cost prewarm removes.
        assert!(
            request_all.contains("join_all"),
            "the AWAIT phase must fan out (join_all) — enqueue-then-await in a \
             single loop is borrow-safe but serialises the whole pre-warm"
        );
        let enqueue_loop_end = request_all[..first_await]
            .rfind("\n        }")
            .expect("the enqueue loop must close before the await phase");
        assert!(
            last_enqueue < enqueue_loop_end && enqueue_loop_end < first_await,
            "the await phase must come after the enqueue loop CLOSES, not inside it"
        );
    }

    /// The pre-warm burst must fan out through `request_all`, never a
    /// `join_all` of `request`.
    ///
    /// On the production transport `request` takes the single `WEB_API` write
    /// borrow and holds it across `api.send().await`. Polling a second probe
    /// into its send while the first is suspended there is a `RefCell`
    /// double-borrow — a PANIC in single-threaded WASM, not contention. The
    /// only thing standing between the old shape and that panic was
    /// `WebApi::send` happening not to yield for small payloads.
    ///
    /// A mock cannot catch this (it holds no shared borrow), so this is a
    /// source pin — the same reason the DM-merge pin above is one.
    #[test]
    fn prewarm_fans_out_through_the_borrow_safe_primitive() {
        let src = include_str!("delegate_migration.rs");
        let production = src
            .split("mod tests {")
            .next()
            .expect("production code before `mod tests`");
        let body = production
            .split("pub(crate) async fn prewarm(")
            .nth(1)
            .expect("prewarm must exist")
            .split("\n    async fn ")
            .next()
            .expect("prewarm body");

        assert!(
            body.contains("request_all("),
            "prewarm must fan out via request_all(), which enqueues sequentially \
             so the WEB_API borrows cannot overlap"
        );
        assert!(
            !body.contains("join_all("),
            "prewarm must not join_all over request() — that can overlap the \
             WEB_API write borrow across send().await (RefCell panic in WASM)"
        );
    }

    // ---------------- concurrency shape ----------------

    /// The crate's walk is strictly sequential (one outstanding request), and
    /// the pre-warm is the one deliberately concurrent phase — with every
    /// request keyed by a DISTINCT target delegate, which is what makes the
    /// burst safe in the single-waiter `PENDING_REQUESTS` map.
    #[test]
    fn walk_is_sequential_and_prewarm_keys_are_distinct() {
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), DelegateSim::default());

        // Pre-warm: all probes in flight together.
        let lineage = lineage3();
        let mut reader = RiverPredecessorIo::new(&channel);
        block_on(reader.prewarm(&lineage));
        assert_eq!(
            channel.max_in_flight.get(),
            3,
            "the pre-warm must probe every predecessor concurrently"
        );

        // The walk itself: never more than one outstanding request.
        channel.reset_counters();
        let mut writer = RiverSuccessorIo::new(&channel, MemorySeam::new(), successor_key());
        let _report = block_on(freenet_migrate::migrate_delegate_secrets(
            &mut writer,
            &mut reader,
            &lineage,
            MigrationAuthorization::app_author_ack(),
            river_secret_policy(),
        ));
        assert!(
            !channel.log.borrow().is_empty(),
            "the walk must actually issue requests"
        );
        assert_eq!(
            channel.max_in_flight.get(),
            1,
            "the walk must hold at most ONE outstanding request — the \
             single-waiter PENDING_REQUESTS safety argument rests on this"
        );

        // The pre-warm's correlation keys are DISTINCT per target delegate.
        // Use the REAL base the pre-warm's `ListRequest` registers under rather
        // than the previous `b"list"` stand-in. This is a READABILITY fix and
        // detects nothing new — the assertions below hold for any base at all.
        // The response side is covered by
        // `request_and_response_correlation_keys_round_trip`, not here.
        let base = chat_delegate::list_request_correlation_key();
        let k0 = chat_delegate::legacy_scoped_correlation(&pred_key(0), &base);
        let k1 = chat_delegate::legacy_scoped_correlation(&pred_key(1), &base);
        let k2 = chat_delegate::legacy_scoped_correlation(&pred_key(2), &base);
        assert!(k0 != k1 && k1 != k2 && k0 != k2);
    }

    // ---------------- the marker contract (freenet-migrate 0.7) ----------------

    /// Rule 1: a data in-progress marker beside an EMPTY done marker reads as
    /// in progress (an import started after the empty seal); a data done marker
    /// always wins; otherwise done wins, else in progress.
    #[test]
    fn marker_pair_reads_follow_rule_1() {
        let d = Some(PRED_DONE_MARKER_VALUE_DATA);
        let e = Some(PRED_DONE_MARKER_VALUE_EMPTY);
        use MigrationMarker::{Done, InProgress};
        assert_eq!(read_marker_pair(None, None), None);
        assert_eq!(
            read_marker_pair(None, e),
            Some(InProgress { saw_data: false })
        );
        assert_eq!(
            read_marker_pair(None, d),
            Some(InProgress { saw_data: true })
        );
        assert_eq!(read_marker_pair(e, None), Some(Done { had_data: false }));
        assert_eq!(read_marker_pair(e, e), Some(Done { had_data: false }));
        assert_eq!(
            read_marker_pair(e, d),
            Some(InProgress { saw_data: true }),
            "rule 1: the empty seal is contradicted by a started import"
        );
        assert_eq!(read_marker_pair(d, None), Some(Done { had_data: true }));
        assert_eq!(read_marker_pair(d, e), Some(Done { had_data: true }));
        assert_eq!(read_marker_pair(d, d), Some(Done { had_data: true }));
    }

    /// Rule 3: an in-progress marker never downgrades data to empty.
    #[test]
    fn wip_marker_writes_follow_rule_3() {
        let d = PRED_DONE_MARKER_VALUE_DATA;
        let e = PRED_DONE_MARKER_VALUE_EMPTY;
        assert_eq!(wip_marker_write(None, false), Some(e));
        assert_eq!(wip_marker_write(None, true), Some(d));
        assert_eq!(
            wip_marker_write(Some(e), true),
            Some(d),
            "upgrade empty to data"
        );
        assert_eq!(wip_marker_write(Some(e), false), None, "already says it");
        assert_eq!(wip_marker_write(Some(d), true), None, "already says it");
        assert_eq!(
            wip_marker_write(Some(d), false),
            None,
            "never downgrade (rule 3)"
        );
    }

    fn writer(channel: &MockChannel) -> RiverSuccessorIo<'_, MockChannel, NoImports> {
        RiverSuccessorIo::new(channel, NoImports, successor_key())
    }

    /// Rules 2 and 3 through River's real store: recording `InProgress` never
    /// erases a `Done`, the pair then reads per rule 1, and an empty in-progress
    /// record never downgrades a data one — including one ANOTHER session
    /// records between this writer's read and its write (the CAS conflict
    /// path: without the compare-and-swap, the empty write would land).
    #[test]
    fn the_marker_store_follows_rules_2_and_3_against_a_concurrent_writer() {
        let pred = pred_key(0);
        let channel = channel_with_gen0(DelegateSim::default());
        let mut w = writer(&channel);

        block_on(w.record_marker(&pred, MigrationMarker::Done { had_data: false })).unwrap();
        block_on(w.record_marker(&pred, MigrationMarker::InProgress { saw_data: true })).unwrap();
        assert!(
            channel
                .stored(&successor_key(), &pred_done_marker_key(&pred))
                .is_some(),
            "rule 2: recording InProgress must not erase Done"
        );
        assert_eq!(
            block_on(w.read_marker(&pred)).unwrap(),
            Some(MigrationMarker::InProgress { saw_data: true }),
            "rule 1 through the real reads"
        );

        // Rule 3, sequential.
        block_on(w.record_marker(&pred, MigrationMarker::InProgress { saw_data: false })).unwrap();
        assert_eq!(
            channel.stored(&successor_key(), &pred_wip_marker_key(&pred)),
            Some(PRED_DONE_MARKER_VALUE_DATA.to_vec())
        );

        // Rule 3 against a concurrent writer: a fresh predecessor, another
        // session writes the data marker between our read and our write.
        let pred1 = pred_key(1);
        channel
            .delegates
            .borrow_mut()
            .get_mut(successor_key().bytes())
            .unwrap()
            .cas_interleave = Some((
            pred_wip_marker_key(&pred1),
            PRED_DONE_MARKER_VALUE_DATA.to_vec(),
        ));
        block_on(w.record_marker(&pred1, MigrationMarker::InProgress { saw_data: false })).unwrap();
        assert_eq!(
            channel.stored(&successor_key(), &pred_wip_marker_key(&pred1)),
            Some(PRED_DONE_MARKER_VALUE_DATA.to_vec()),
            "rule 3: a concurrent session's data marker must survive our empty write"
        );
    }

    // ---------------- the journal's storage ----------------

    /// `load_journal` is `Err` on silence (never `None`, which would make a
    /// later store start the journal afresh over a stored one), and
    /// `store_journal` is `Err` until the delegate acknowledges.
    #[test]
    fn journal_io_is_loud_on_silence() {
        let successor = DelegateSim {
            silent: true,
            ..Default::default()
        };
        let channel = MockChannel::default().with_delegate(&successor_key(), successor);
        let mut w = writer(&channel);
        assert!(block_on(w.load_journal()).is_err());
        assert!(block_on(w.store_journal(b"x")).is_err());

        let channel = channel_with_gen0(DelegateSim::default());
        let mut w = writer(&channel);
        assert_eq!(
            block_on(w.load_journal()).unwrap(),
            None,
            "answered absence"
        );
        block_on(w.store_journal(b"journal-bytes")).unwrap();
        assert_eq!(
            channel.stored(&successor_key(), MIGRATION_JOURNAL_KEY),
            Some(b"journal-bytes".to_vec()),
            "stored under the crate's reserved key"
        );
        assert_eq!(
            block_on(w.load_journal()).unwrap(),
            Some(b"journal-bytes".to_vec())
        );
        assert_eq!(w.import_revision(), RIVER_IMPORT_REVISION);
        const {
            assert!(
                RIVER_IMPORT_REVISION >= 1,
                "raised with the byte-string decoder change (freenet/river#757)"
            )
        };
    }

    /// The journaled walk stores the journal and resolves what it imported;
    /// a second run's status is the same and re-fetches nothing.
    #[test]
    fn the_journaled_walk_resolves_and_stores_the_journal() {
        let channel = channel_with_gen0(gen0_with_blob());
        let (first, _) = walk_j(&channel, MemorySeam::new());
        let status = first.status.expect("journal stored");
        assert!(status.is_complete(), "{status:?}");
        assert!(!status.interrupted_import());
        assert!(channel
            .stored(&successor_key(), MIGRATION_JOURNAL_KEY)
            .is_some());

        channel.reset_counters();
        let (second, seam) = walk_j(&channel, MemorySeam::new());
        assert_eq!(second.status, Some(status));
        assert_eq!(channel.requests_to_predecessors("get:"), 0);
        assert!(seam.rooms.map.is_empty());
    }

    /// A predecessor's own migration bookkeeping (its journal, its markers, its
    /// cadence counter: what this delegate will hold once it is a predecessor)
    /// is never fetched, imported or counted.
    #[test]
    fn a_predecessors_bookkeeping_is_never_fetched_or_imported() {
        let mut gen0 = gen0_with_blob();
        for key in [
            MIGRATION_JOURNAL_KEY.to_vec(),
            WALK_CADENCE_KEY.to_vec(),
            TOPUP_WINDOW_KEY.to_vec(),
            pred_done_marker_key(&DelegateKey::new([9; 32], CodeHash::new([9; 32]))),
        ] {
            gen0.store.insert(key, b"bookkeeping".to_vec());
        }
        let channel = channel_with_gen0(gen0);
        let (report, seam) = walk(&channel, MemorySeam::new());
        assert!(report.is_complete(), "{report:?}");
        assert!(seam.rooms.map.contains_key(&vk(1)));
        let log = channel.log.borrow();
        for key in [MIGRATION_JOURNAL_KEY, WALK_CADENCE_KEY, TOPUP_WINDOW_KEY] {
            let asked = format!("get:{}", String::from_utf8_lossy(key));
            assert!(
                !log.iter()
                    .any(|(target, what)| *target == pred_key(0).bytes() && *what == asked),
                "{asked} must never be fetched from a predecessor"
            );
        }
    }

    // ---------------- the sweep's journal gate ----------------

    #[test]
    fn sweep_write_mapping() {
        assert_eq!(sweep_write_for(&Ok(AppImport::Flagged)), SweepWrite::Merge);
        assert_eq!(
            sweep_write_for(&Ok(AppImport::AlreadyImported)),
            SweepWrite::MergeUnflagged
        );
        assert_eq!(
            sweep_write_for(&Err(JournalError::NotImportable("x".into()))),
            SweepWrite::Skip,
            "never permanent: skip this load only"
        );
        assert_eq!(
            sweep_write_for(&Err(JournalError::Successor("x".into()))),
            SweepWrite::Skip
        );
    }

    /// A lineage of `n` generations, every predecessor installed and empty.
    fn lineage_n(n: u8) -> (Vec<DelegateLineageEntry>, MockChannel) {
        let table: Vec<([u8; 32], [u8; 32])> =
            (0..n).map(|g| ([20 + g; 32], [120 + g; 32])).collect();
        let mut channel =
            MockChannel::default().with_delegate(&successor_key(), DelegateSim::default());
        for (k, ch) in &table {
            channel = channel.with_delegate(
                &DelegateKey::new(*k, CodeHash::new(*ch)),
                DelegateSim::default(),
            );
        }
        (river_delegate_lineage(&table), channel)
    }

    /// freenet/river#809: `begin` read every predecessor's markers one round
    /// trip at a time (done, then in-progress), so on the first load after a
    /// re-key the sweep's merge waited 2 x 33 round trips for it: about 8 s of
    /// the 16 s the rehearsal measured before any room showed. The marker reads
    /// now go out in bounded concurrent waves, so `begin` costs a handful of
    /// round trips whatever the lineage length.
    #[test]
    fn begin_reads_markers_in_waves_not_one_round_trip_each() {
        let rounds = |n: u8| {
            let (lineage, channel) = lineage_n(n);
            block_on(begin_with(&channel, successor_key(), &lineage)).unwrap();
            channel.round_trips.get()
        };
        let small = rounds(3);
        let large = rounds(33);
        let extra_waves = (2 * 33usize).div_ceil(MARKER_PREFETCH_WAVE)
            - (2 * 3usize).div_ceil(MARKER_PREFETCH_WAVE);
        assert!(
            large <= small + extra_waves,
            "begin over 33 generations took {large} round trips against {small} over 3: \
             its marker reads must not grow one round trip per generation"
        );
    }

    /// With a journal stored, `begin` is off the visible path and reads few
    /// markers, so it does not prefetch them all.
    #[test]
    fn begin_prefetches_only_before_the_journal_exists() {
        let (lineage, channel) = lineage_n(5);
        block_on(begin_with(&channel, successor_key(), &lineage)).unwrap();
        let marker_reads = |c: &MockChannel| {
            c.log
                .borrow()
                .iter()
                .filter(|(_, what)| what.starts_with("get:__migrate_pred_"))
                .count()
        };
        assert_eq!(
            marker_reads(&channel),
            10,
            "first begin: every marker, prefetched"
        );
        channel.reset_counters();
        block_on(begin_with(&channel, successor_key(), &lineage)).unwrap();
        assert_eq!(
            marker_reads(&channel),
            0,
            "nothing resolved yet, so nothing to re-read"
        );
    }

    /// The prefetch never turns silence into absence (constraint 1): a marker
    /// the prefetch could not read is read again live, and a marker that stays
    /// silent still refuses `begin`, exactly as without the prefetch.
    #[test]
    fn begin_still_refuses_on_a_marker_that_stays_silent() {
        let (lineage, channel) = lineage_n(3);
        let silent = pred_done_marker_key(&DelegateKey::new(
            lineage[1].delegate_key,
            CodeHash::new(lineage[1].code_hash),
        ));
        channel
            .delegates
            .borrow_mut()
            .get_mut(successor_key().bytes())
            .unwrap()
            .silent_get_keys
            .push(silent.clone());
        assert!(block_on(begin_with(&channel, successor_key(), &lineage)).is_err());
        let reads = channel
            .log
            .borrow()
            .iter()
            .filter(|(_, what)| *what == format!("get:{}", String::from_utf8_lossy(&silent)))
            .count();
        assert_eq!(reads, 2, "read once by the prefetch, then again live");
    }

    /// A prefetched marker is used at most once and is dropped by a write, so
    /// the in-progress marker's read-back (the crate's marker contract) sees
    /// what was just recorded, never the value read before it.
    #[test]
    fn a_prefetched_marker_does_not_outlive_a_write() {
        let channel = channel_with_gen0(DelegateSim::default());
        let lineage = lineage3();
        let mut io = RiverSuccessorIo::new(&channel, NoImports, successor_key());
        block_on(io.prefetch_markers(&lineage));
        let pred = pred_key(0);
        block_on(io.record_marker(&pred, MigrationMarker::InProgress { saw_data: true })).unwrap();
        assert_eq!(
            block_on(io.read_marker(&pred)).unwrap(),
            Some(MigrationMarker::InProgress { saw_data: true })
        );
    }

    fn note(channel: &MockChannel, gen: usize, items: Option<u32>) -> SweepWrite {
        block_on(note_sweep_import_with(
            channel,
            successor_key(),
            &lineage3(),
            pred_key(gen).bytes(),
            items,
        ))
    }

    /// The sweep's whole journal cycle: refused before `begin`; flagged (the
    /// journal and the in-progress marker) once begun, so a kill now leaves an
    /// interrupted import; resolved by the walk; then merged unflagged (the backstop for a
    /// completed import that was partial). The predecessor key comes from the
    /// lineage, so the crate never sees a key "not in the lineage".
    #[test]
    fn the_sweep_flags_before_merging_and_the_walk_resolves_it() {
        let channel = channel_with_gen0(gen0_with_blob());
        assert_eq!(
            note(&channel, 0, None),
            SweepWrite::Skip,
            "no journal yet: begin must come first"
        );
        let begun = block_on(begin_with(&channel, successor_key(), &lineage3())).unwrap();
        assert!(!begun.interrupted_import());

        assert_eq!(note(&channel, 0, None), SweepWrite::Merge);
        assert_eq!(
            channel.stored(&successor_key(), &pred_wip_marker_key(&pred_key(0))),
            Some(PRED_DONE_MARKER_VALUE_DATA.to_vec()),
            "the flag is also the predecessor's in-progress marker"
        );
        // A kill here: the next load's begin reports the interrupted import.
        let next_load = block_on(begin_with(&channel, successor_key(), &lineage3())).unwrap();
        assert!(next_load.interrupted_import());

        let (walked, seam) = walk_j(&channel, MemorySeam::new());
        let status = walked.status.expect("status");
        assert!(!status.interrupted_import(), "{status:?}");
        assert!(seam.rooms.map.contains_key(&vk(1)), "the walk imported it");

        assert_eq!(
            note(&channel, 0, None),
            SweepWrite::MergeUnflagged,
            "a completed import is still merged, unflagged, through the ranked merge"
        );

        // A delegate outside the lineage is never merged.
        assert_eq!(
            block_on(note_sweep_import_with(
                &channel,
                successor_key(),
                &lineage3(),
                &[0x77; 32],
                None
            )),
            SweepWrite::Skip
        );
    }

    /// freenet-migrate#47: an interrupted sweep import counted 2 items; the
    /// walk's retry then sees the predecessor export only 1 (a truncated
    /// index). It must report `exported_fewer_after_data` and stay pending —
    /// never resolve `Imported` over a partial copy.
    #[test]
    fn an_interrupted_sweep_then_a_short_retry_is_not_believed() {
        let room_a = vk(1);
        let room_b = vk(2);
        let mut gen0 = DelegateSim::default();
        for (room, seed) in [(room_a, 3), (room_b, 4)] {
            gen0.store.insert(
                chat_delegate::room_storage_key(&room),
                cbor(&RoomSlot::Present(Box::new(room_with_identity(room, seed)))),
            );
        }
        let channel = channel_with_gen0(gen0);
        block_on(begin_with(&channel, successor_key(), &lineage3())).unwrap();
        assert_eq!(note(&channel, 0, Some(2)), SweepWrite::Merge);

        // The retry: room_b's index entry is gone (a truncated index).
        {
            let mut delegates = channel.delegates.borrow_mut();
            let sim = delegates.get_mut(pred_key(0).bytes()).unwrap();
            sim.listed = Some(vec![chat_delegate::room_storage_key(&room_a)]);
        }
        let (walked, _seam) = walk_j(&channel, MemorySeam::new());
        assert!(
            walked
                .report
                .predecessors
                .iter()
                .any(|p| p.generation() == 0 && p.exported_fewer_after_data()),
            "the short retry must be reported: {:?}",
            walked.report
        );
        let status = walked.status.expect("status");
        assert!(
            status.interrupted_import(),
            "still pending with its import flagged, never Imported: {status:?}"
        );
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(0))),
            None
        );
    }

    /// The headline residual (freenet/river#763): an interrupted import, then
    /// the predecessor's index secret is gone on the retry. With a fixed key
    /// stored, `fetch_secrets` itself refuses (an absent index beside stored
    /// data). With only per-room keys an absent index looks exactly like an
    /// empty delegate, and the crate's "nothing after data" rule refuses it.
    /// Either way the predecessor never resolves `Imported`.
    #[test]
    fn an_interrupted_import_then_a_lost_index_never_resolves_imported() {
        for with_fixed_key in [true, false] {
            let mut gen0 = DelegateSim::default();
            gen0.store.insert(
                chat_delegate::room_storage_key(&vk(2)),
                cbor(&RoomSlot::Present(Box::new(room_with_identity(vk(2), 4)))),
            );
            if with_fixed_key {
                gen0.store.insert(
                    OUTBOUND_DMS_STORAGE_KEY.to_vec(),
                    cbor(&OutboundDmStore::default()),
                );
            }
            let channel = channel_with_gen0(gen0);
            block_on(begin_with(&channel, successor_key(), &lineage3())).unwrap();
            assert_eq!(note(&channel, 0, Some(1)), SweepWrite::Merge);
            {
                let mut delegates = channel.delegates.borrow_mut();
                let sim = delegates.get_mut(pred_key(0).bytes()).unwrap();
                sim.listed = Some(vec![]);
                sim.raw_index = Some(None);
            }
            let (walked, _) = walk_j(&channel, MemorySeam::new());
            let status = walked.status.expect("status");
            assert!(
                status.interrupted_import(),
                "with_fixed_key={with_fixed_key}: must stay an interrupted import: {status:?}"
            );
            assert_eq!(
                channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(0))),
                None,
                "with_fixed_key={with_fixed_key}"
            );
        }
    }

    /// The predecessor of the #760 reproduction: four rooms as per-room slots.
    fn gen0_with_four_rooms() -> (DelegateSim, [VerifyingKey; 4]) {
        let rooms = [vk(1), vk(2), vk(3), vk(4)];
        let mut gen0 = DelegateSim::default();
        for (i, room) in rooms.iter().enumerate() {
            gen0.store.insert(
                chat_delegate::room_storage_key(room),
                cbor(&RoomSlot::Present(Box::new(room_with_identity(
                    *room,
                    10 + i as u8,
                )))),
            );
        }
        (gen0, rooms)
    }

    /// The journal half of the load after an interrupted one, as the driver
    /// runs it: `begin`, the #761 walk decision (with the cadence counter at
    /// any value it could hold), and the walk when the decision says to.
    fn next_load(channel: &MockChannel, seam: MemorySeam) -> (JournalStatus, MemorySeam) {
        use super::super::migration_journal::{load_plan, walk_decision};
        let begun = block_on(begin_with(channel, successor_key(), &lineage3())).unwrap();
        assert!(
            begun.interrupted_import(),
            "the next load's begin must report the interrupted import: {begun:?}"
        );
        for skips in [None, Some(0), Some(1), Some(8), Some(u32::MAX)] {
            let plan = load_plan(walk_decision(&begun, &[], skips), false);
            assert!(
                plan.walk,
                "an interrupted import walks on every load (skips={skips:?})"
            );
        }
        let (walked, seam) = walk_j(channel, seam);
        (walked.status.expect("status"), seam)
    }

    /// freenet/river#760, as reproduced: four rooms in the predecessor, the
    /// first load after the re-key killed right after the sweep's first
    /// per-room write. The successor then lists one room, so the next load's
    /// plan is `PerRoom` and the old recovery (a `localStorage` flag the
    /// gateway's sandboxed iframe never stored) never ran. Here the flag is in
    /// the successor's journal: the next load's `begin` reports it, the walk
    /// runs whatever the cadence counter says, and every room arrives.
    ///
    /// This pins the mechanism (journal flag, walk decision, walk) on the mock
    /// channel; the successor's one room is the reloaded seam. The load-plan
    /// classification and the wasm-only drivers are pinned by source tests in
    /// `response_handler.rs` and `migration_journal.rs`, and the end-to-end
    /// interrupt runs were done on a real node for #768.
    #[test]
    fn a_tab_killed_after_the_first_room_write_strands_no_room() {
        let (gen0, rooms) = gen0_with_four_rooms();
        let channel = channel_with_gen0(gen0);

        // Load 1: begin, then the sweep flags its import and writes one room
        // before the tab dies.
        block_on(begin_with(&channel, successor_key(), &lineage3())).unwrap();
        assert_eq!(note(&channel, 0, Some(4)), SweepWrite::Merge);
        let mut reloaded = MemorySeam::new();
        reloaded
            .rooms
            .map
            .insert(rooms[0], room_with_identity(rooms[0], 10));

        // Load 2 starts from that one room.
        let (status, seam) = next_load(&channel, reloaded);
        for room in rooms {
            assert!(
                seam.rooms.map.contains_key(&room),
                "room {room:?} stranded in the predecessor"
            );
        }
        assert!(!status.interrupted_import(), "{status:?}");
        assert!(pred_resolved(&status, &pred_key(0)), "{status:?}");

        // Load 3: nothing is interrupted any more.
        let load3 = block_on(begin_with(&channel, successor_key(), &lineage3())).unwrap();
        assert!(!load3.interrupted_import(), "{load3:?}");
    }

    /// The same, with the kill during the walk's own import: its merges were
    /// made but its save never landed (the tab died before the flush). The
    /// crate flagged the import before its first write, so the next load
    /// walks again and imports every room into a successor that holds none.
    #[test]
    fn a_walk_killed_before_its_save_landed_strands_no_room() {
        let (gen0, rooms) = gen0_with_four_rooms();
        let channel = channel_with_gen0(gen0);

        block_on(begin_with(&channel, successor_key(), &lineage3())).unwrap();
        let mut dying = MemorySeam::new();
        dying.fail_flush = true;
        let (first, lost) = walk_j(&channel, dying);
        assert_eq!(lost.rooms.map.len(), 4, "the walk's merges were made");
        assert!(
            first
                .status
                .as_ref()
                .is_some_and(|s| s.interrupted_import()),
            "a walk whose save failed must not resolve the predecessor: {:?}",
            first.status
        );
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(0))),
            None
        );

        let (status, seam) = next_load(&channel, MemorySeam::new());
        for room in rooms {
            assert!(
                seam.rooms.map.contains_key(&room),
                "room {room:?} stranded in the predecessor"
            );
        }
        assert!(!status.interrupted_import(), "{status:?}");
        assert!(pred_resolved(&status, &pred_key(0)), "{status:?}");
    }

    /// The #761 cadence never delays that recovery. Here every generation,
    /// including the one holding the rooms, was silent for enough walks to be
    /// rate-limited, so without the import flag the next load would skip the
    /// walk. The sweep then reaches it, flags, and the tab dies after one room:
    /// the next load must still walk. (Silence, not typed absence, for the
    /// other generations: `NOT_INSTALLED` is process-global, and these are the
    /// `table3` keys other tests share.)
    #[test]
    fn an_interrupted_import_walks_even_where_the_cadence_would_skip() {
        use super::super::migration_journal::{walk_cadence, SILENT_MIN_ATTEMPTS};
        let (gen0, rooms) = gen0_with_four_rooms();
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(
                &pred_key(1),
                DelegateSim {
                    silent: true,
                    ..Default::default()
                },
            )
            .with_delegate(
                &pred_key(2),
                DelegateSim {
                    silent: true,
                    ..Default::default()
                },
            )
            .with_delegate(&successor_key(), DelegateSim::default());

        channel.silence(&pred_key(0), true);
        for _ in 0..SILENT_MIN_ATTEMPTS {
            block_on(begin_with(&channel, successor_key(), &lineage3())).unwrap();
            let _ = walk_j(&channel, MemorySeam::new());
        }
        let settled = block_on(begin_with(&channel, successor_key(), &lineage3())).unwrap();
        assert!(
            walk_cadence(&settled, &[]).is_some(),
            "fixture: the cadence must rate-limit this status: {settled:?}"
        );

        channel.silence(&pred_key(0), false);
        assert_eq!(note(&channel, 0, Some(4)), SweepWrite::Merge);
        let mut reloaded = MemorySeam::new();
        reloaded
            .rooms
            .map
            .insert(rooms[0], room_with_identity(rooms[0], 10));

        let (status, seam) = next_load(&channel, reloaded);
        for room in rooms {
            assert!(
                seam.rooms.map.contains_key(&room),
                "room {room:?} stranded in the predecessor"
            );
        }
        assert!(!status.interrupted_import(), "{status:?}");
        assert!(pred_resolved(&status, &pred_key(0)), "{status:?}");
    }

    /// Whether `pred` is no longer pending in `status`.
    fn pred_resolved(status: &JournalStatus, pred: &DelegateKey) -> bool {
        match status {
            JournalStatus::Complete { .. } => true,
            JournalStatus::Incomplete { pending, .. } => {
                pending.iter().all(|p| p.key.bytes() != pred.bytes())
            }
            _ => false,
        }
    }

    // ---------------- ranked merge: deletions, and Union shadowing ----------

    /// "A room deleted after migration is not resurrected" (freenet/river#763's
    /// gate for merging an already-imported predecessor again): the current
    /// delegate's tombstone (no rank recorded = authoritative) outranks every
    /// legacy generation's `Present`.
    #[test]
    fn a_room_deleted_after_migration_is_not_resurrected() {
        let left = vk(1);
        let channel = channel_with_gen0(gen0_with_blob()); // gen0 holds `left` Present
        let mut seam = MemorySeam::new();
        seam.rooms.removed_rooms.insert(left);
        let (_report, seam) = walk(&channel, seam);
        assert!(
            !seam.rooms.map.contains_key(&left),
            "a legacy copy must not bring back a room the user left"
        );
        assert!(seam.rooms.removed_rooms.contains(&left));
    }

    /// freenet-migrate#42 under Union: a refused newest generation (silent on
    /// the first walk) lets an older generation's copy of a room be imported
    /// first. Within the session River's ranked merge still lets the newer
    /// generation win when it answers: precedence is generation RANK, not
    /// arrival. (Across a page load the older copy reloads at the current
    /// delegate's rank; that residual is documented in the module docs.)
    #[test]
    fn a_refused_newest_generation_does_not_shadow_it_within_the_session() {
        let room = vk(5);
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(room, 50));
        let mut gen2 = DelegateSim::default();
        gen2.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(room, 52));
        gen2.silent = true;
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());

        let (_first, seam) = walk(&channel, MemorySeam::new());
        assert_eq!(
            seam.rooms.map[&room].self_verifying_key().unwrap(),
            SigningKey::from_bytes(&[50; 32]).verifying_key(),
            "gen0's copy lands first while gen2 is silent"
        );

        channel
            .delegates
            .borrow_mut()
            .get_mut(pred_key(2).bytes())
            .unwrap()
            .silent = false;
        let (_second, seam) = walk(&channel, seam);
        assert_eq!(
            seam.rooms.map[&room].self_verifying_key().unwrap(),
            SigningKey::from_bytes(&[52; 32]).verifying_key(),
            "the newer generation's identity must win once it answers (rank, not arrival)"
        );
    }

    // ---------------- "not installed" ----------------

    /// A typed "missing delegate" makes the probe `NotInstalled`, which the
    /// journal counts in the absent streak the freenet/river#761 cadence reads;
    /// plain silence never does.
    #[test]
    fn a_typed_missing_delegate_counts_toward_the_absent_streak() {
        let status = all_absent_status(3);
        assert!(status.pending_all_absent(3), "{status:?}");

        let gen0 = DelegateSim {
            silent: true,
            ..Default::default()
        };
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&successor_key(), DelegateSim::default());
        let lineage = river_delegate_lineage(&table3()[..1]);
        let mut last = None;
        for _ in 0..3 {
            let (report, _) = block_on(walk_with(&channel, NoImports, successor_key(), &lineage));
            last = report.status;
        }
        assert!(
            !last.expect("status").pending_all_absent(1),
            "silence is never 'not installed'"
        );
    }

    // ---------------- probes added by the testing review of PR #768 ----------------

    /// The in-progress marker's CAS must retry at the FRESH generation. Another
    /// session records the weaker (empty) marker between our read and our
    /// write: the conflict's value is weaker than ours, so we must write again,
    /// and a stale generation would conflict until the attempts run out.
    #[test]
    fn a_wip_marker_conflict_with_a_weaker_value_is_retried_at_the_fresh_generation() {
        let pred = pred_key(0);
        let channel = channel_with_gen0(DelegateSim::default());
        channel
            .delegates
            .borrow_mut()
            .get_mut(successor_key().bytes())
            .unwrap()
            .cas_interleave = Some((
            pred_wip_marker_key(&pred),
            PRED_DONE_MARKER_VALUE_EMPTY.to_vec(),
        ));
        let mut w = writer(&channel);
        block_on(w.record_marker(&pred, MigrationMarker::InProgress { saw_data: true }))
            .expect("a conflict is retried at the generation it reported");
        assert_eq!(
            channel.stored(&successor_key(), &pred_wip_marker_key(&pred)),
            Some(PRED_DONE_MARKER_VALUE_DATA.to_vec()),
            "the data marker must land over the concurrent empty one"
        );
    }

    /// The freenet/river#761 cadence counter: absent reads 0, the value
    /// round-trips (little-endian, under its own key), and a value that is not
    /// 4 bytes or a silent delegate is an `Err` (which walks), never a count.
    #[test]
    fn the_walk_cadence_counter_round_trips_and_fails_closed() {
        let s = successor_key();
        let channel = channel_with_gen0(DelegateSim::default());
        assert_eq!(block_on(read_walk_cadence(&channel, &s)), Ok(0));
        block_on(store_walk_cadence(&channel, &s, 0x0102_0304)).unwrap();
        assert_eq!(
            channel.stored(&s, WALK_CADENCE_KEY),
            Some(0x0102_0304u32.to_le_bytes().to_vec())
        );
        assert_eq!(block_on(read_walk_cadence(&channel, &s)), Ok(0x0102_0304));
        channel.put(&s, WALK_CADENCE_KEY, &[1, 2, 3]);
        assert!(block_on(read_walk_cadence(&channel, &s)).is_err());

        let silent = MockChannel::default().with_delegate(
            &s,
            DelegateSim {
                silent: true,
                ..Default::default()
            },
        );
        assert!(block_on(read_walk_cadence(&silent, &s)).is_err());
        assert!(block_on(store_walk_cadence(&silent, &s, 1)).is_err());
    }

    /// `get_marker`'s own refusal arms. With the WHOLE successor silent the
    /// journal read fails first and masks them (the older test of this shape
    /// stays green with both arms mutated to `Ok(None)`), so only the marker
    /// read is made unreadable here: silent, or answered with another key's
    /// name. Guessing "no marker" would re-import a possibly-done predecessor;
    /// accepting the other key's value would seal it.
    #[test]
    fn an_unreadable_marker_stops_the_walk_even_when_the_journal_is_readable() {
        let done = pred_done_marker_key(&pred_key(0));
        for (what, successor) in [
            (
                "silent marker read",
                DelegateSim {
                    silent_get_keys: vec![done.clone()],
                    ..Default::default()
                },
            ),
            (
                "marker read answered with another key's name",
                DelegateSim {
                    wrong_key_reply: Some((
                        done.clone(),
                        pred_done_marker_key(&pred_key(1)),
                        PRED_DONE_MARKER_VALUE_DATA.to_vec(),
                    )),
                    ..Default::default()
                },
            ),
        ] {
            let channel = MockChannel::default()
                .with_delegate(&pred_key(0), gen0_with_blob())
                .with_delegate(&pred_key(1), DelegateSim::default())
                .with_delegate(&pred_key(2), DelegateSim::default())
                .with_delegate(&successor_key(), successor);
            let (report, seam) = walk(&channel, MemorySeam::new());
            assert!(
                matches!(
                    report.predecessors.first(),
                    Some(PredecessorMigration::WriterUnavailable { .. })
                ),
                "{what}: must stop the walk, got {report:?}"
            );
            assert!(
                seam.rooms.map.is_empty(),
                "{what}: nothing may be imported blind"
            );
            assert_eq!(
                channel.requests_to_predecessors("get:"),
                0,
                "{what}: no predecessor fetch may run without marker bookkeeping"
            );
        }
    }

    /// What the journal makes of the two ways an item can fail. Bytes that do
    /// not decode are a PERMANENT rejection: counted (the identity-import gate
    /// blocks on it, and a raised `RIVER_IMPORT_REVISION` reopens it) and not
    /// retried. A merge or save that fails is RETRYABLE: the predecessor stays
    /// pending, nothing is counted rejected, and the next load tries again.
    #[test]
    fn an_undecodable_item_is_a_counted_permanent_rejection() {
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), vec![0xFF, 0x00, 0x13]);
        let channel = channel_with_gen0(gen0);
        let (journaled, seam) = walk_j(&channel, MemorySeam::new());
        let status = journaled.status.expect("status");
        assert_eq!(status.rejected_items(), 1, "{status:?}");
        assert!(seam.rooms.map.is_empty());
        let session = super::super::migration_journal::JournalSession {
            begun: true,
            status: Some(status),
            ..Default::default()
        };
        assert_eq!(
            super::super::migration_journal::import_gate(&session, false),
            super::super::migration_journal::ImportGate::Blocked,
            "rejected items must hold the identity-import gate"
        );
    }

    #[test]
    fn a_failed_merge_is_retried_not_rejected() {
        let channel = channel_with_gen0(gen0_with_blob());
        let mut seam = MemorySeam::new();
        seam.fail_merge = Some("merge refused".to_string());
        let (journaled, _seam) = walk_j(&channel, seam);
        let status = journaled.status.expect("status");
        assert_eq!(
            status.rejected_items(),
            0,
            "a retryable failure is not a rejection: {status:?}"
        );
        assert!(!journaled.report.is_complete());
        assert!(journaled.report.retry_may_help());
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(0))),
            None,
            "a predecessor whose merge failed must not be sealed"
        );

        // And the next load, with the merge working, imports it.
        let (_second, seam) = walk_j(&channel, MemorySeam::new());
        assert!(seam.rooms.map.contains_key(&vk(1)));
    }

    #[test]
    fn a_failed_dm_merge_is_retried_not_rejected() {
        let mut gen0 = DelegateSim::default();
        gen0.store.insert(
            OUTBOUND_DMS_STORAGE_KEY.to_vec(),
            cbor(&OutboundDmStore::default()),
        );
        let channel = channel_with_gen0(gen0);
        let mut seam = MemorySeam::new();
        seam.fail_merge = Some("dm merge refused".to_string());
        let (journaled, _seam) = walk_j(&channel, seam);
        let status = journaled.status.expect("status");
        assert_eq!(status.rejected_items(), 0, "{status:?}");
        assert!(journaled.report.retry_may_help());
        assert_eq!(
            channel.stored(&successor_key(), &pred_done_marker_key(&pred_key(0))),
            None
        );
    }

    /// The journal's and the markers' stores are durable only on the delegate's
    /// OWN acknowledgement: a refusal, or an acknowledgement for another key,
    /// is an error. Swallowing either would let a marker or the journal read
    /// as recorded when it is not.
    #[test]
    fn a_refused_or_misaddressed_store_ack_is_an_error() {
        let pred = pred_key(0);
        for (what, successor) in [
            (
                "refused",
                DelegateSim {
                    store_error: Some("secret store full".to_string()),
                    ..Default::default()
                },
            ),
            (
                "acknowledged under another key",
                DelegateSim {
                    store_ack_key: Some(b"some-other-key".to_vec()),
                    ..Default::default()
                },
            ),
        ] {
            let channel = MockChannel::default().with_delegate(&successor_key(), successor);
            let mut w = writer(&channel);
            assert!(
                block_on(w.store_journal(b"j")).is_err(),
                "{what}: journal store"
            );
            assert!(
                block_on(w.record_marker(&pred, MigrationMarker::Done { had_data: true })).is_err(),
                "{what}: done marker store"
            );
            assert!(
                block_on(store_walk_cadence(&channel, &successor_key(), 3)).is_err(),
                "{what}: cadence"
            );
        }
    }

    /// A read answered for another key is a fault, never the value: the journal
    /// is the authority every decision rests on.
    #[test]
    fn a_journal_read_answered_for_another_key_is_an_error() {
        let successor = DelegateSim {
            wrong_key_reply: Some((
                MIGRATION_JOURNAL_KEY.to_vec(),
                b"some-other-key".to_vec(),
                b"not-the-journal".to_vec(),
            )),
            ..Default::default()
        };
        let channel = MockChannel::default().with_delegate(&successor_key(), successor);
        let mut w = writer(&channel);
        assert!(block_on(w.load_journal()).is_err());
        assert!(
            block_on(read_walk_cadence(&channel, &successor_key())).is_ok(),
            "other keys unaffected"
        );
    }

    /// A page load as the driver composes it: `begin`, a sweep merge that the
    /// journal flags, the walk that resolves it, with the session updated by
    /// the same transitions the driver applies. The gate must read Open on a
    /// fresh node, Blocked from the flag until the walk's status knows about
    /// it, and Open again; and a tab killed in between must be reported by the
    /// next load's `begin`.
    #[test]
    fn a_page_load_moves_the_gate_open_blocked_open() {
        use super::super::migration_journal::{
            import_gate, sweep_write_raises_flag, ImportGate, JournalSession,
        };
        let channel = channel_with_gen0(gen0_with_blob());
        let mut session = JournalSession::default();
        assert_eq!(import_gate(&session, false), ImportGate::Pending);

        let status = block_on(begin_with(&channel, successor_key(), &lineage3())).unwrap();
        session.on_begin_ok(status);
        assert_eq!(
            import_gate(&session, false),
            ImportGate::Unresolved,
            "no predecessor has answered yet: the import waits (overridably) for them"
        );

        let write = note(&channel, 0, None);
        assert_eq!(write, SweepWrite::Merge);
        assert!(sweep_write_raises_flag(write));
        session.sweep_flagged_since_status = true;
        assert_eq!(import_gate(&session, false), ImportGate::Blocked);

        // A kill here: the next load's begin reports the interrupted import and
        // the recovery answer leans to re-running the sweep.
        let mut next_load = JournalSession::default();
        next_load
            .on_begin_ok(block_on(begin_with(&channel, successor_key(), &lineage3())).unwrap());
        assert_eq!(import_gate(&next_load, false), ImportGate::Blocked);
        assert_eq!(next_load.recovery_answer(), Some(true));

        // Without a kill: the walk resolves it and its status clears the flag.
        session.walk_in_flight = true;
        assert_eq!(import_gate(&session, false), ImportGate::Pending);
        let (walked, seam) = walk_j(&channel, MemorySeam::new());
        assert!(seam.rooms.map.contains_key(&vk(1)));
        session.walk_in_flight = false;
        session.on_walk_done(walked.status);
        assert_eq!(import_gate(&session, false), ImportGate::Open);
        assert_eq!(session.recovery_answer(), Some(false));
    }

    /// `rooms_meta` carries the user's per-room notification choices as well as
    /// the order: both must survive an import, and the legacy cursor must not.
    #[test]
    fn rooms_to_merge_keeps_notification_modes_and_order_but_not_the_cursor() {
        let room = vk(1);
        let meta = RecoveredItem::Meta(RoomsMeta {
            current_room_key: Some(vk(2)),
            notification_modes: HashMap::from([(room, crate::room_data::NotificationMode::Muted)]),
            room_order: vec![room],
            identity_sources: Vec::new(),
        });
        let rooms = rooms_to_merge(&meta).expect("meta merges");
        assert!(matches!(
            rooms.notification_modes.get(&room),
            Some(crate::room_data::NotificationMode::Muted)
        ));
        assert_eq!(rooms.room_order, vec![room]);
        assert!(rooms.current_room_key.is_none());
    }

    /// "Not installed" evidence belongs to ONE pre-warm burst. A typed answer
    /// left over from an earlier burst (the sweep's probes, a previous walk)
    /// must not turn this burst's plain silence into absence: that would feed
    /// the freenet/river#761 cadence, which then walks one load in ten, for a
    /// predecessor that is merely silent.
    #[test]
    fn evidence_of_an_earlier_burst_does_not_turn_silence_into_absence() {
        // Keys no other test uses: `NOT_INSTALLED` is process-global.
        let table = [([0xC1u8; 32], [0xD1u8; 32])];
        let key = DelegateKey::new(table[0].0, CodeHash::new(table[0].1));
        note_legacy_delegate_not_installed(&key);
        let channel = MockChannel::default()
            .with_delegate(
                &key,
                DelegateSim {
                    silent: true,
                    ..Default::default()
                },
            )
            .with_delegate(&successor_key(), DelegateSim::default());
        let lineage = river_delegate_lineage(&table);
        let mut last = None;
        for _ in 0..3 {
            let (report, _) = block_on(walk_with(&channel, NoImports, successor_key(), &lineage));
            last = report.status;
        }
        assert!(
            !last.expect("status").pending_all_absent(1),
            "silence in this burst is silence, whatever an earlier burst heard"
        );
    }

    /// freenet/river#763 review A1: the live seam re-checks the room set
    /// before every merge and before its flush, so a reconnect or a load
    /// failure DURING a walk stops it merging into, saving over, or sealing on
    /// top of an incomplete set. The seam touches Dioxus signals, so it is
    /// pinned in source; `room_set_complete_core` is tested in chat_delegate.
    #[test]
    fn the_live_seam_rechecks_the_room_set_before_merging_and_flushing() {
        let src = include_str!("delegate_migration.rs");
        let production = src.split("mod tests_support {").next().expect("production");
        let seam = production
            .find("impl RiverImportSeam for LiveImportSeam {")
            .expect("live seam");
        let seam = &production[seam..];
        let seam = &seam[..seam.find("\n}\n").expect("impl ends")];
        for f in [
            "async fn merge_rooms(",
            "async fn merge_outbound_dms(",
            "async fn flush(",
        ] {
            let at = seam.find(f).unwrap_or_else(|| panic!("{f}"));
            let body = &seam[at..];
            let first_stmt = body.find('{').expect("body") + 1;
            assert!(
                body[first_stmt..]
                    .trim_start()
                    .starts_with("self.ensure_room_set_complete()?;"),
                "{f} must refuse before doing anything"
            );
        }
        assert!(production.contains("chat_delegate::room_set_complete(self.attempt)"));
        // ...and again INSIDE each deferred mutation, before it touches a
        // signal (#768 round 2 item 5): a reconnect between the check and the
        // macrotask must not let a superseded attempt write.
        for (f, mutation) in [
            ("async fn merge_rooms(", "ROOMS.with_mut("),
            (
                "async fn merge_outbound_dms(",
                "hydrate_outbound_dms_cache_now(",
            ),
        ] {
            let at = seam.find(f).unwrap();
            let body = &seam[at..];
            let defer = body.find("crate::util::defer(move ||").expect("defer");
            let recheck = body[defer..]
                .find("if !chat_delegate::room_set_complete(attempt) {")
                .map(|i| defer + i)
                .unwrap_or_else(|| panic!("{f}: re-check inside the defer"));
            let write = body.find(mutation).expect("mutation");
            assert!(
                defer < recheck && recheck < write,
                "{f}: re-check before the write"
            );
        }
    }

    /// #768 review A4 (migration probe P1): a frozen generation's corrupt
    /// index became empty, and its next store rewrote it as a one-key index.
    /// Listing and index agree and are both short, so only a probed key that
    /// is stored but unlisted can prove it. Refused, never certified.
    #[test]
    fn a_rewritten_short_index_is_refused_by_a_stored_unlisted_fixed_key() {
        let (a, b) = (vk(1), vk(2));
        let slot = |r, seed| cbor(&RoomSlot::Present(Box::new(room_with_identity(r, seed))));
        for witness in [OUTBOUND_DMS_STORAGE_KEY, ROOMS_META_KEY] {
            let mut gen0 = DelegateSim::default();
            gen0.store
                .insert(chat_delegate::room_storage_key(&a), slot(a, 3));
            gen0.store
                .insert(chat_delegate::room_storage_key(&b), slot(b, 4));
            let value = if witness == ROOMS_META_KEY {
                cbor(&RoomsMeta {
                    current_room_key: None,
                    notification_modes: HashMap::new(),
                    room_order: vec![a, b],
                    identity_sources: Vec::new(),
                })
            } else {
                cbor(&OutboundDmStore::default())
            };
            gen0.store.insert(witness.to_vec(), value);
            gen0.listed = Some(vec![chat_delegate::room_storage_key(&b)]);
            let channel = channel_with_gen0(gen0);
            let (report, seam) = walk(&channel, MemorySeam::new());
            assert_gen0_refused(&report, &channel, &seam);
        }
    }

    /// #768 round 2 item 9 (migration probe R1b), the COMMON #592 shape: the
    /// short index was rewritten by a later `rooms_meta` store, so it lists
    /// `rooms_meta` itself and the fixed-key check passes, while room A's slot
    /// is stored but unlisted. `rooms_meta` names A (in its order, its
    /// notification modes, or its cursor): the probe of A's slot refuses the
    /// export. A named room whose slot is genuinely absent is not a fault.
    #[test]
    fn a_short_index_that_lists_rooms_meta_is_refused_by_a_named_room() {
        use crate::room_data::NotificationMode;
        let (a, b) = (vk(1), vk(2));
        let slot = |r, seed| cbor(&RoomSlot::Present(Box::new(room_with_identity(r, seed))));
        let metas = [
            RoomsMeta {
                room_order: vec![b, a],
                ..Default::default()
            },
            RoomsMeta {
                notification_modes: HashMap::from([(a, NotificationMode::default())]),
                ..Default::default()
            },
            RoomsMeta {
                current_room_key: Some(a),
                ..Default::default()
            },
        ];
        for meta in metas {
            let mut gen0 = DelegateSim::default();
            gen0.store
                .insert(chat_delegate::room_storage_key(&a), slot(a, 3));
            gen0.store
                .insert(chat_delegate::room_storage_key(&b), slot(b, 4));
            gen0.store.insert(ROOMS_META_KEY.to_vec(), cbor(&meta));
            gen0.listed = Some(vec![
                chat_delegate::room_storage_key(&b),
                ROOMS_META_KEY.to_vec(),
            ]);
            let channel = channel_with_gen0(gen0);
            let (report, seam) = walk(&channel, MemorySeam::new());
            assert_gen0_refused_with_salvage(&report, &channel, &seam, &[a], &[b]);
        }

        // Control: rooms_meta names A, but A's slot is genuinely gone.
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(chat_delegate::room_storage_key(&b), slot(b, 4));
        gen0.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                room_order: vec![b, a],
                ..Default::default()
            }),
        );
        let channel = channel_with_gen0(gen0);
        let (report, seam) = walk(&channel, MemorySeam::new());
        assert!(report.is_complete(), "{report:?}");
        assert!(seam.rooms.map.contains_key(&b));
    }

    /// #768 review A4 (migration probe P2): an absent index beside per-room
    /// keys and a stored `rooms_meta` is a read fault, not an empty delegate:
    /// `rooms_meta` is always probed, so it witnesses the fault.
    #[test]
    fn an_absent_index_beside_a_stored_rooms_meta_is_a_fault() {
        let a = vk(1);
        let mut gen0 = DelegateSim::default();
        gen0.store.insert(
            chat_delegate::room_storage_key(&a),
            cbor(&RoomSlot::Present(Box::new(room_with_identity(a, 3)))),
        );
        gen0.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                current_room_key: None,
                notification_modes: HashMap::new(),
                room_order: vec![],
                identity_sources: Vec::new(),
            }),
        );
        gen0.listed = Some(vec![]);
        gen0.raw_index = Some(None);
        let channel = channel_with_gen0(gen0);
        let (report, seam) = walk(&channel, MemorySeam::new());
        assert_gen0_refused(&report, &channel, &seam);
    }

    /// freenet/river#768 review B3 (migration probe P5), ACROSS a page load:
    /// V33 (gen 2) is silent on the first burst and an older generation's
    /// identity for a room is imported and saved; on the next load the room
    /// reloads from the current delegate, and V33 then answers with its newer
    /// identity. With the persisted identity source restored, the newer key
    /// wins. The control (no restore: what reloading at the current rank did
    /// before) keeps the older key, which is the bug.
    #[test]
    fn a_newer_generations_identity_wins_across_a_page_load() {
        let room = vk(5);
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(room, 50));
        let mut gen2 = DelegateSim::default();
        gen2.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(room, 52));
        gen2.silent = true;
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());

        // Load 1: gen0's identity lands and is saved, with its source.
        let (_first, seam) = walk(&channel, MemorySeam::new());
        let current_rank = table3().len() as u32;
        let sources = crate::room_data::persisted_identity_sources(
            &seam.rooms,
            &seam.ranks.identity,
            &table3(),
            current_rank,
        );
        assert_eq!(sources, vec![(room, table3()[0].0)]);

        // Load 2: V33 answers now.
        channel
            .delegates
            .borrow_mut()
            .get_mut(pred_key(2).bytes())
            .unwrap()
            .silent = false;
        let reload = |restore: bool| {
            let mut s = MemorySeam::new();
            s.rooms = seam.rooms.clone();
            // The current delegate's load records its rooms at its own rank.
            s.ranks.identity.insert(room.to_bytes(), current_rank);
            if restore {
                crate::room_data::restore_identity_ranks(
                    &mut s.ranks.identity,
                    &sources,
                    &table3(),
                    current_rank,
                );
            }
            s
        };
        let (_second, restored) = walk(&channel, reload(true));
        assert_eq!(
            restored.rooms.map[&room].self_verifying_key().unwrap(),
            SigningKey::from_bytes(&[52; 32]).verifying_key(),
            "V33's newer identity must win on the later load"
        );

        // Control: without the persisted source (the pre-B3 reload), the older
        // key shadows the newer one for good.
        let channel2 = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), {
                let mut g = DelegateSim::default();
                g.store
                    .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(room, 52));
                g
            })
            .with_delegate(&successor_key(), DelegateSim::default());
        let (_c, unrestored) = walk(&channel2, reload(false));
        assert_eq!(
            unrestored.rooms.map[&room].self_verifying_key().unwrap(),
            SigningKey::from_bytes(&[50; 32]).verifying_key(),
            "control: reloaded at the current rank, the older key is kept"
        );
    }

    /// #768 round 2 (Consider): the same restore lets a newer generation's
    /// TOMBSTONE, answering on a later load, remove a room an older
    /// generation supplied: the room's restored rank is the older one, and a
    /// newer generation's tombstone outranks it. (A room the user touched in
    /// the new build is stamped at the current rank and is not removed.)
    #[test]
    fn a_newer_generations_tombstone_wins_across_a_page_load() {
        let room = vk(5);
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(room, 50));
        let mut gen2 = DelegateSim::default();
        gen2.store.insert(
            chat_delegate::room_storage_key(&room),
            cbor(&RoomSlot::Tombstone),
        );
        gen2.silent = true;
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());
        let (_first, seam) = walk(&channel, MemorySeam::new());
        assert!(seam.rooms.map.contains_key(&room));
        let current_rank = table3().len() as u32;
        let sources = crate::room_data::persisted_identity_sources(
            &seam.rooms,
            &seam.ranks.identity,
            &table3(),
            current_rank,
        );
        channel
            .delegates
            .borrow_mut()
            .get_mut(pred_key(2).bytes())
            .unwrap()
            .silent = false;
        let reload = |restore: bool| {
            let mut s = MemorySeam::new();
            s.rooms = seam.rooms.clone();
            s.ranks.identity.insert(room.to_bytes(), current_rank);
            if restore {
                crate::room_data::restore_identity_ranks(
                    &mut s.ranks.identity,
                    &sources,
                    &table3(),
                    current_rank,
                );
            }
            s
        };
        let (_second, restored) = walk(&channel, reload(true));
        assert!(
            !restored.rooms.map.contains_key(&room),
            "V33's later tombstone removes the room gen0 supplied"
        );
    }

    // ---------------- the post-resolve top-up (review B1) ----------------

    /// An old tab still on the previous build creates a room in V33 (gen 2)
    /// AFTER the journal resolved V33. The walk never reads V33 again; the
    /// top-up does, and imports it through the ranked merge. A room the user
    /// deleted in the new build is NOT brought back (migration probe P3).
    #[test]
    fn the_topup_imports_an_old_tabs_room_and_never_resurrects_a_deleted_one() {
        let (a, c, d) = (vk(1), vk(3), vk(4));
        let slot = |r, seed| cbor(&RoomSlot::Present(Box::new(room_with_identity(r, seed))));
        let mut gen2 = DelegateSim::default();
        gen2.store
            .insert(chat_delegate::room_storage_key(&a), slot(a, 3));
        gen2.store
            .insert(chat_delegate::room_storage_key(&d), slot(d, 6));
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());
        let (first, mut seam) = walk_j(&channel, MemorySeam::new());
        assert!(first.status.expect("status").is_complete());
        assert!(seam.rooms.map.contains_key(&a) && seam.rooms.map.contains_key(&d));

        // The user leaves room D in the new build.
        seam.rooms.map.remove(&d);
        seam.rooms.removed_rooms.insert(d);
        // An old tab creates room C in V33, and writes D again.
        channel.put(
            &pred_key(2),
            &chat_delegate::room_storage_key(&c),
            &slot(c, 9),
        );
        channel.put(
            &pred_key(2),
            &chat_delegate::room_storage_key(&d),
            &slot(d, 7),
        );

        // The walk does not see it: V33 is resolved.
        let (_again, seam) = walk_j(&channel, seam);
        assert!(!seam.rooms.map.contains_key(&c));

        // The top-up does.
        let mut seam = seam;
        let merged = block_on(top_up_from_with(&channel, &mut seam, &lineage3()[2])).unwrap();
        assert_eq!(merged, 1, "only the room the successor does not know");
        assert!(
            seam.rooms.map.contains_key(&c),
            "the old tab's room is imported"
        );
        assert!(
            !seam.rooms.map.contains_key(&d) && seam.rooms.removed_rooms.contains(&d),
            "a room deleted in the new build stays deleted"
        );
        assert!(seam.flushes > 0, "the top-up persists what it merged");

        // Idempotent: a second top-up finds nothing new.
        let again = block_on(top_up_from_with(&channel, &mut seam, &lineage3()[2])).unwrap();
        assert_eq!(again, 0);
    }

    /// #768 round 2 item 8: the window counts only top-ups that RAN, ends
    /// after `TOPUP_WINDOW_LOADS` of them, and restarts when the predecessor's
    /// index changes, so an old tab's room created after the window "ended"
    /// is still imported. A silent predecessor counts nothing.
    #[test]
    fn the_topup_window_counts_runs_and_restarts_when_the_index_changes() {
        use super::super::migration_journal::{TOPUP_WINDOW_LOADS, TOPUP_WINDOW_MS};
        let slot = |r, seed| cbor(&RoomSlot::Present(Box::new(room_with_identity(r, seed))));
        let (a, c) = (vk(1), vk(3));
        let mut gen2 = DelegateSim::default();
        gen2.store
            .insert(chat_delegate::room_storage_key(&a), slot(a, 3));
        let channel = MockChannel::default()
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());
        let newest = &lineage3()[2];
        let t0 = 5_000_000u64;
        let run = |seam: &mut MemorySeam, now| {
            block_on(top_up_in_window_with(
                &channel,
                seam,
                &successor_key(),
                newest,
                now,
            ))
        };

        // A silent predecessor: an error, and nothing recorded.
        channel.silence(&pred_key(2), true);
        let mut seam = MemorySeam::new();
        assert!(run(&mut seam, t0).is_err());
        assert_eq!(
            block_on(read_topup_window(&channel, &successor_key())).unwrap(),
            None,
            "a top-up that did not run is not counted"
        );
        channel.silence(&pred_key(2), false);
        // The index answers but the listing does not: still not a run.
        channel
            .delegates
            .borrow_mut()
            .get_mut(pred_key(2).bytes())
            .unwrap()
            .silent_list = true;
        assert!(run(&mut seam, t0).is_err());
        assert_eq!(
            block_on(read_topup_window(&channel, &successor_key())).unwrap(),
            None,
            "a top-up whose listing went unanswered is not counted"
        );
        channel
            .delegates
            .borrow_mut()
            .get_mut(pred_key(2).bytes())
            .unwrap()
            .silent_list = false;

        // Runs are counted until the window ends.
        for i in 1..=TOPUP_WINDOW_LOADS {
            match run(&mut seam, t0 + u64::from(i)).unwrap() {
                TopUpOutcome::Ran { window, .. } => assert_eq!(window.runs, i),
                TopUpOutcome::Ended => panic!("ended early at run {i}"),
            }
        }
        assert!(seam.rooms.map.contains_key(&a));
        assert_eq!(run(&mut seam, t0 + 100).unwrap(), TopUpOutcome::Ended);

        // Long after, an old tab creates room C: the index changed, so the
        // window restarts and C is imported.
        let later = t0 + TOPUP_WINDOW_MS * 2;
        channel.put(
            &pred_key(2),
            &chat_delegate::room_storage_key(&c),
            &slot(c, 9),
        );
        match run(&mut seam, later).unwrap() {
            TopUpOutcome::Ran { merged, window } => {
                assert_eq!(merged, 1);
                assert_eq!((window.started_ms, window.runs), (later, 1));
            }
            TopUpOutcome::Ended => panic!("a changed index must restart the window"),
        }
        assert!(seam.rooms.map.contains_key(&c));
    }

    /// #768 round 2 item 6: the top-up merges the predecessor's DM entries,
    /// never its `hidden_threads`, so a thread the user un-archived in the new
    /// build is not archived again on every load of the window.
    #[test]
    fn the_topup_merges_dm_entries_but_never_hidden_threads() {
        use freenet_scaffold::util::FastHash;
        use river_core::room_state::member::MemberId;
        let room = vk(1);
        let store = OutboundDmStore {
            entries: vec![river_core::chat_delegate::OutboundDmEntry {
                room_owner_vk: room.to_bytes(),
                sender: MemberId(FastHash(1)),
                recipient: MemberId(FastHash(2)),
                purge_token: river_core::room_state::direct_messages::PurgeToken([7; 16]),
                timestamp: 10,
                plaintext: "hi".into(),
            }],
            hidden_threads: vec![river_core::chat_delegate::HiddenDmThreadEntry {
                room_owner_vk: room.to_bytes(),
                peer: MemberId(FastHash(2)),
                hidden_at_ts: 10,
            }],
        };
        let channel = MockChannel::default().with_delegate(&pred_key(2), {
            let mut g = DelegateSim::default();
            g.store
                .insert(OUTBOUND_DMS_STORAGE_KEY.to_vec(), cbor(&store));
            g
        });
        let mut seam = MemorySeam::new();
        let merged = block_on(top_up_from_with(&channel, &mut seam, &lineage3()[2])).unwrap();
        assert_eq!(merged, 1);
        assert_eq!(seam.dm_merges.len(), 1);
        assert_eq!(
            seam.dm_merges[0].entries, store.entries,
            "entries are topped up"
        );
        assert!(
            seam.dm_merges[0].hidden_threads.is_empty(),
            "hidden threads are never re-merged by the top-up"
        );
    }

    /// #768 round 4 nit: when the stored short-index record cannot be read,
    /// a salvage does not write one back over it (other predecessors' digests
    /// would be lost).
    #[test]
    fn an_unreadable_short_index_record_is_not_overwritten() {
        let (a, b) = (vk(1), vk(2));
        let slot = |r, seed| cbor(&RoomSlot::Present(Box::new(room_with_identity(r, seed))));
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(chat_delegate::room_storage_key(&a), slot(a, 3));
        gen0.store
            .insert(chat_delegate::room_storage_key(&b), slot(b, 4));
        gen0.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                room_order: vec![b, a],
                ..Default::default()
            }),
        );
        gen0.listed = Some(vec![
            chat_delegate::room_storage_key(&b),
            ROOMS_META_KEY.to_vec(),
        ]);
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), {
                let mut succ = DelegateSim::default();
                succ.silent_get_keys = vec![SHORT_INDEX_KEY.to_vec()];
                succ
            });
        let prior: ShortIndexRecord = vec![([9u8; 32], [8u8; 32])];
        let mut bytes = Vec::new();
        ciborium::into_writer(&prior, &mut bytes).unwrap();
        channel.put(&successor_key(), SHORT_INDEX_KEY, &bytes);
        let (_, seam) = walk(&channel, MemorySeam::new());
        assert!(seam.rooms.map.contains_key(&a), "the salvage still runs");
        assert_eq!(
            channel.stored(&successor_key(), SHORT_INDEX_KEY),
            Some(bytes),
            "the unreadable record must not be replaced"
        );
    }

    /// #768 round 4, S-C: a short-index predecessor the sweep flagged stays
    /// mid-import for good. Once its refusal is recorded (and its rooms
    /// salvaged), it counts toward the silent cadence instead of forcing a walk
    /// on every load; before SILENT_MIN_ATTEMPTS walks, or unrecorded, it walks.
    #[test]
    fn a_recorded_short_index_refusal_counts_toward_the_cadence() {
        use super::super::migration_journal::{
            walk_cadence, SILENT_MIN_ATTEMPTS, SILENT_WALK_EVERY_N_LOADS,
        };
        let (a, b) = (vk(1), vk(2));
        let slot = |r, seed| cbor(&RoomSlot::Present(Box::new(room_with_identity(r, seed))));
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(chat_delegate::room_storage_key(&a), slot(a, 3));
        gen0.store
            .insert(chat_delegate::room_storage_key(&b), slot(b, 4));
        gen0.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                room_order: vec![b, a],
                ..Default::default()
            }),
        );
        gen0.listed = Some(vec![
            chat_delegate::room_storage_key(&b),
            ROOMS_META_KEY.to_vec(),
        ]);
        let channel = channel_with_gen0(gen0);
        // The sweep flagged an import from gen 0 (its marker is data).
        channel.put(
            &successor_key(),
            &pred_wip_marker_key(&pred_key(0)),
            freenet_migrate::PRED_DONE_MARKER_VALUE_DATA,
        );
        let mut status = None;
        for walk_no in 1..=SILENT_MIN_ATTEMPTS {
            let (journaled, _) = walk_j(&channel, MemorySeam::new());
            let st = journaled.status.expect("status");
            assert!(st.interrupted_import(), "it stays mid-import");
            let recorded: Vec<[u8; 32]> =
                block_on(read_short_index_record(&channel, &successor_key()))
                    .unwrap()
                    .into_iter()
                    .map(|(k, _)| k)
                    .collect();
            assert_eq!(recorded.len(), 1, "the refusal is recorded");
            assert_eq!(walk_cadence(&st, &[]), None, "unrecorded: walk every load");
            if walk_no < SILENT_MIN_ATTEMPTS {
                assert_eq!(
                    walk_cadence(&st, &recorded),
                    None,
                    "walk {walk_no}: not yet"
                );
            }
            status = Some((st, recorded));
        }
        let (st, recorded) = status.unwrap();
        assert_eq!(
            walk_cadence(&st, &recorded),
            Some(SILENT_WALK_EVERY_N_LOADS)
        );
    }

    /// #768 round 4, M1: the top-up merges DM entries without touching the
    /// archive state: the newest predecessor's archive set (which the walk
    /// already imported once, if it was the newest holder) is never merged
    /// again, and an archive already in the successor is left alone.
    #[test]
    fn a_topup_never_touches_archive_state() {
        use freenet_scaffold::util::FastHash;
        use river_core::room_state::member::MemberId;
        let room = vk(1);
        let (peer, other) = (MemberId(FastHash(2)), MemberId(FastHash(3)));
        let hidden = |p| river_core::chat_delegate::HiddenDmThreadEntry {
            room_owner_vk: room.to_bytes(),
            peer: p,
            hidden_at_ts: 5,
        };
        let store = OutboundDmStore {
            entries: vec![river_core::chat_delegate::OutboundDmEntry {
                room_owner_vk: room.to_bytes(),
                sender: MemberId(FastHash(1)),
                recipient: peer,
                purge_token: river_core::room_state::direct_messages::PurgeToken([7; 16]),
                timestamp: 10,
                plaintext: "hi".into(),
            }],
            // An old tab archived `other` in the predecessor after it resolved.
            hidden_threads: vec![hidden(other)],
        };
        let channel = MockChannel::default().with_delegate(&pred_key(2), {
            let mut g = DelegateSim::default();
            g.store
                .insert(OUTBOUND_DMS_STORAGE_KEY.to_vec(), cbor(&store));
            g
        });
        let mut seam = MemorySeam::new();
        seam.hidden.insert((room, peer), hidden(peer));
        let merged = block_on(top_up_from_with(&channel, &mut seam, &lineage3()[2])).unwrap();
        assert_eq!(merged, 1, "the DM entry is topped up");
        assert!(
            seam.hidden.contains_key(&(room, peer)),
            "an archive is left alone"
        );
        assert!(
            !seam.hidden.contains_key(&(room, other)),
            "the top-up merges no archive entries"
        );
    }

    /// #768 round 3, N2: an unchanged predecessor's DM entries cost no merge
    /// and no save on each top-up run.
    #[test]
    fn a_topup_with_nothing_new_merges_and_saves_nothing() {
        use freenet_scaffold::util::FastHash;
        use river_core::room_state::member::MemberId;
        let room = vk(1);
        let store = OutboundDmStore {
            entries: vec![river_core::chat_delegate::OutboundDmEntry {
                room_owner_vk: room.to_bytes(),
                sender: MemberId(FastHash(1)),
                recipient: MemberId(FastHash(2)),
                purge_token: river_core::room_state::direct_messages::PurgeToken([7; 16]),
                timestamp: 10,
                plaintext: "hi".into(),
            }],
            hidden_threads: Vec::new(),
        };
        let channel = MockChannel::default().with_delegate(&pred_key(2), {
            let mut g = DelegateSim::default();
            g.store
                .insert(OUTBOUND_DMS_STORAGE_KEY.to_vec(), cbor(&store));
            g
        });
        struct HoldsAll(MemorySeam);
        impl RiverImportSeam for HoldsAll {
            async fn merge_rooms(&mut self, r: Rooms, rank: u32) -> Result<(), String> {
                self.0.merge_rooms(r, rank).await
            }
            async fn merge_outbound_dms(
                &mut self,
                s: OutboundDmStore,
                rank: Option<u32>,
            ) -> Result<(), String> {
                self.0.merge_outbound_dms(s, rank).await
            }
            async fn new_outbound_entries(
                &mut self,
                _entries: Vec<river_core::chat_delegate::OutboundDmEntry>,
            ) -> Vec<river_core::chat_delegate::OutboundDmEntry> {
                Vec::new()
            }
            async fn flush(&mut self) -> Result<(), String> {
                self.0.flush().await
            }
        }
        let mut seam = HoldsAll(MemorySeam::new());
        let merged = block_on(top_up_from_with(&channel, &mut seam, &lineage3()[2])).unwrap();
        assert_eq!(merged, 0);
        assert!(seam.0.dm_merges.is_empty(), "nothing merged");
        assert_eq!(seam.0.flushes, 0, "nothing saved");
    }

    /// Fixture for the #776 journal rule: gen 1 (older) archived K; gen 2
    /// (newer) holds `newer_hidden` in a DM store of its own, or nothing at
    /// all (`None`: no data, so the journal resolves it as holding none).
    fn archive_fixture(
        newer_hidden: Option<Vec<river_core::chat_delegate::HiddenDmThreadEntry>>,
    ) -> (
        MockChannel,
        (VerifyingKey, river_core::room_state::member::MemberId),
        river_core::chat_delegate::HiddenDmThreadEntry,
    ) {
        use freenet_scaffold::util::FastHash;
        let room = vk(1);
        let peer = river_core::room_state::member::MemberId(FastHash(2));
        let k = river_core::chat_delegate::HiddenDmThreadEntry {
            room_owner_vk: room.to_bytes(),
            peer,
            hidden_at_ts: 10,
        };
        let with_dms = |hidden: Vec<river_core::chat_delegate::HiddenDmThreadEntry>| {
            let mut g = DelegateSim::default();
            g.store.insert(
                OUTBOUND_DMS_STORAGE_KEY.to_vec(),
                cbor(&OutboundDmStore {
                    entries: Vec::new(),
                    hidden_threads: hidden,
                }),
            );
            g
        };
        let gen2 = match newer_hidden {
            Some(h) => with_dms(h),
            None => DelegateSim::default(),
        };
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), with_dms(vec![k.clone()]))
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());
        (channel, (room, peer), k)
    }

    /// One page load: a fresh walk over the previous load's archive state.
    fn archive_load(channel: &MockChannel, prev: &MemorySeam) -> MemorySeam {
        let mut seam = MemorySeam::new();
        seam.hidden = prev.hidden.clone();
        walk(channel, seam).1
    }

    /// #776 journal rule within one load: the newest generation holding data
    /// is the only one whose archives merge. A newer generation with data and
    /// no K keeps K out; a newer one that answered with nothing, or that is
    /// typed "not installed", lets the older generation's K in.
    #[test]
    fn only_the_newest_data_holding_generation_merges_archives() {
        let (channel, key, _) = archive_fixture(Some(vec![]));
        let seam = archive_load(&channel, &MemorySeam::new());
        assert!(
            !seam.hidden.contains_key(&key),
            "a newer data holder lacking K wins"
        );

        let (channel, key, _) = archive_fixture(None);
        let seam = archive_load(&channel, &MemorySeam::new());
        assert!(
            seam.hidden.contains_key(&key),
            "a newer generation holding nothing defers"
        );

        // Not installed: keys no other test uses, because `NOT_INSTALLED` is
        // process-global and another walk's pre-warm clears it for its own
        // targets (shared keys made this case flaky).
        let (_, key, k) = archive_fixture(None);
        let table = [
            ([0x71u8; 32], [0xf1u8; 32]),
            ([0x72; 32], [0xf2; 32]),
            ([0x73; 32], [0xf3; 32]),
        ];
        let lineage = river_delegate_lineage(&table);
        let key_of = |i: usize| DelegateKey::new(table[i].0, CodeHash::new(table[i].1));
        let mut older = DelegateSim::default();
        older.store.insert(
            OUTBOUND_DMS_STORAGE_KEY.to_vec(),
            cbor(&OutboundDmStore {
                entries: Vec::new(),
                hidden_threads: vec![k],
            }),
        );
        let channel = MockChannel::default()
            .with_delegate(&key_of(0), DelegateSim::default())
            .with_delegate(&key_of(1), older)
            .with_delegate(
                &key_of(2),
                DelegateSim {
                    not_installed: true,
                    ..Default::default()
                },
            )
            .with_delegate(&successor_key(), DelegateSim::default());
        let (_, seam) = block_on(walk_with(
            &channel,
            MemorySeam::new(),
            successor_key(),
            &lineage,
        ));
        assert!(
            seam.hidden.contains_key(&key),
            "a newer generation not installed defers"
        );
    }

    /// The pure #776 rule, one disjunct at a time. Journals come from real
    /// walks over one 2-generation lineage (gen 0 the candidate, gen 1 newer),
    /// each on its own mock node, on keys no other test uses (`NOT_INSTALLED`
    /// is process-global).
    #[test]
    fn newest_archive_holder_table() {
        use freenet_migrate::MigrationJournal;
        let table = [([0x81u8; 32], [0x01u8; 32]), ([0x82; 32], [0x02; 32])];
        let pairs = table.to_vec();
        let lineage = river_delegate_lineage(&table);
        let journal_after = |newer: DelegateSim| -> MigrationJournal {
            let channel = MockChannel::default()
                .with_delegate(
                    &DelegateKey::new(table[0].0, CodeHash::new(table[0].1)),
                    DelegateSim::default(),
                )
                .with_delegate(
                    &DelegateKey::new(table[1].0, CodeHash::new(table[1].1)),
                    newer,
                )
                .with_delegate(&successor_key(), DelegateSim::default());
            let _ = block_on(walk_with(
                &channel,
                MemorySeam::new(),
                successor_key(),
                &lineage,
            ));
            let bytes = channel
                .stored(&successor_key(), MIGRATION_JOURNAL_KEY)
                .expect("journal stored");
            MigrationJournal::decode(&bytes).expect("journal decodes")
        };
        let r = vk(9);
        let mut with_room = DelegateSim::default();
        with_room.store.insert(
            chat_delegate::room_storage_key(&r),
            cbor(&RoomSlot::Present(Box::new(room_with_identity(r, 5)))),
        );
        let data = journal_after(with_room);
        let nodata = journal_after(DelegateSim::default());
        let absent = journal_after(DelegateSim {
            not_installed: true,
            ..Default::default()
        });
        let silent = journal_after(DelegateSim {
            silent: true,
            ..Default::default()
        });
        let newer = DelegateKey::new(table[1].0, CodeHash::new(table[1].1));
        let state = |j: &MigrationJournal| j.entry(&newer).map(|e| e.state);
        assert!(
            matches!(state(&data), Some(freenet_migrate::JournalState::Resolved(r)) if r.is_data_bearing()),
            "fixture: {:?}",
            state(&data)
        );
        assert!(
            matches!(state(&nodata), Some(freenet_migrate::JournalState::Resolved(r)) if !r.is_data_bearing()),
            "fixture: {:?}",
            state(&nodata)
        );
        assert!(
            matches!(state(&absent), Some(freenet_migrate::JournalState::Pending { absent_streak, .. }) if absent_streak > 0),
            "fixture: {:?}",
            state(&absent)
        );
        assert!(
            matches!(
                state(&silent),
                Some(freenet_migrate::JournalState::Pending {
                    absent_streak: 0,
                    ..
                })
            ),
            "fixture: {:?}",
            state(&silent)
        );
        let none = |_: &[u8; 32]| None;
        let held = |_: &[u8; 32]| Some(true);
        let empty = |_: &[u8; 32]| Some(false);
        let no = |_: &[u8; 32]| false;
        let yes = |_: &[u8; 32]| true;
        let holder = |j: Option<&MigrationJournal>,
                      a: &dyn Fn(&[u8; 32]) -> Option<bool>,
                      n: &dyn Fn(&[u8; 32]) -> bool| {
            newest_archive_holder(0, &pairs, j, a, n)
        };
        // This walk's answer decides first, over the journal.
        assert!(
            holder(Some(&data), &empty, &no),
            "answered without a DM store: defer"
        );
        assert!(
            !holder(Some(&nodata), &held, &no),
            "answered with a DM store: block"
        );
        // The journal's resolution.
        assert!(
            holder(Some(&nodata), &none, &no),
            "resolved, not data-bearing: defer"
        );
        assert!(
            !holder(Some(&data), &none, &no),
            "resolved, data-bearing: block"
        );
        // Pending: typed not installed, by the streak or by this pre-warm.
        assert!(holder(Some(&absent), &none, &no), "absent streak: defer");
        assert!(
            holder(Some(&silent), &none, &yes),
            "pre-warm not installed: defer"
        );
        assert!(!holder(Some(&silent), &none, &no), "silent: block");
        // No journal: block. The newest generation itself: nothing newer.
        assert!(!holder(None, &none, &yes), "no journal entry: block");
        assert!(newest_archive_holder(1, &pairs, None, &none, &no));
    }

    /// #776: archive "data" means a DM store. A newer generation that holds
    /// rooms but no DM store never recorded an archive decision (archiving and
    /// un-archiving both save the DM store), so the older generation's archive
    /// merges past it.
    #[test]
    fn a_newer_generation_with_rooms_but_no_dm_store_lets_older_archives_in() {
        let (channel, key, _) = archive_fixture(None);
        let r = vk(9);
        channel.put(
            &pred_key(2),
            &chat_delegate::room_storage_key(&r),
            &cbor(&RoomSlot::Present(Box::new(room_with_identity(r, 5)))),
        );
        let seam = archive_load(&channel, &MemorySeam::new());
        assert!(
            seam.rooms.map.contains_key(&r),
            "the newer generation's room imported"
        );
        assert!(seam.hidden.contains_key(&key), "the older archive merges");
    }

    /// #776 across page loads, NEWER generation silent first: load 1 must not
    /// merge the older generation's K (the newer one may hold data), and once
    /// the newer one answers in load 2 its set is the one that counts.
    #[test]
    fn a_silent_newer_generation_keeps_older_archives_out_across_loads() {
        // The newer generation un-archived K.
        let (channel, key, _) = archive_fixture(Some(vec![]));
        channel.silence(&pred_key(2), true);
        let load1 = archive_load(&channel, &MemorySeam::new());
        assert!(
            !load1.hidden.contains_key(&key),
            "load 1: the older K is held back"
        );
        channel.silence(&pred_key(2), false);
        let load2 = archive_load(&channel, &load1);
        assert!(
            !load2.hidden.contains_key(&key),
            "load 2: the un-archive stands"
        );
        // The newer generation still archives K: it arrives with it.
        let (channel, key, k) = archive_fixture(None);
        channel.put(
            &pred_key(2),
            OUTBOUND_DMS_STORAGE_KEY,
            &cbor(&OutboundDmStore {
                entries: Vec::new(),
                hidden_threads: vec![k],
            }),
        );
        channel.silence(&pred_key(2), true);
        let load1 = archive_load(&channel, &MemorySeam::new());
        channel.silence(&pred_key(2), false);
        let load2 = archive_load(&channel, &load1);
        assert!(
            load2.hidden.contains_key(&key),
            "the newer generation's archive arrives"
        );
    }

    /// #776 across page loads, OLDER generation silent first: the newer one
    /// resolves holding data in load 1; when the older one answers in load 2
    /// its K stays out.
    #[test]
    fn an_older_generation_answering_later_never_resurrects_an_archive() {
        let (channel, key, _) = archive_fixture(Some(vec![]));
        channel.silence(&pred_key(1), true);
        let load1 = archive_load(&channel, &MemorySeam::new());
        assert!(!load1.hidden.contains_key(&key));
        channel.silence(&pred_key(1), false);
        let load2 = archive_load(&channel, &load1);
        assert!(
            !load2.hidden.contains_key(&key),
            "the older K must not come back"
        );
    }

    /// freenet/river#776 through the real walk: the newest predecessor (gen 2)
    /// no longer holds an archive entry (the user un-archived it there), an
    /// older one (gen 1) still does. The walk passes each generation's rank,
    /// so the entry stays deleted.
    #[test]
    fn the_walk_never_resurrects_an_archive_entry_a_newer_generation_deleted() {
        use freenet_scaffold::util::FastHash;
        use river_core::room_state::member::MemberId;
        let room = vk(1);
        let peer = MemberId(FastHash(2));
        let store = |hidden: Vec<river_core::chat_delegate::HiddenDmThreadEntry>| OutboundDmStore {
            entries: Vec::new(),
            hidden_threads: hidden,
        };
        let archived = river_core::chat_delegate::HiddenDmThreadEntry {
            room_owner_vk: room.to_bytes(),
            peer,
            hidden_at_ts: 10,
        };
        let with_dms = |s: OutboundDmStore| {
            let mut g = DelegateSim::default();
            g.store.insert(OUTBOUND_DMS_STORAGE_KEY.to_vec(), cbor(&s));
            g
        };
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), with_dms(store(vec![archived.clone()])))
            .with_delegate(&pred_key(2), with_dms(store(vec![])))
            .with_delegate(&successor_key(), DelegateSim::default());
        let (report, seam) = walk(&channel, MemorySeam::new());
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(
            seam.dm_merges.len(),
            2,
            "both generations' stores were imported"
        );
        assert!(
            seam.hidden.is_empty(),
            "an older generation's archive entry must not come back: {:?}",
            seam.hidden
        );
    }

    /// Even when a deleted room IS offered (a seam that knows nothing), the
    /// ranked merge keeps the current delegate's tombstone.
    #[test]
    fn a_topped_up_copy_never_outranks_a_current_tombstone() {
        let d = vk(4);
        let channel = MockChannel::default().with_delegate(&pred_key(2), {
            let mut g = DelegateSim::default();
            g.store.insert(
                chat_delegate::room_storage_key(&d),
                cbor(&RoomSlot::Present(Box::new(room_with_identity(d, 6)))),
            );
            g
        });
        struct Blind(MemorySeam);
        impl RiverImportSeam for Blind {
            async fn merge_rooms(&mut self, r: Rooms, rank: u32) -> Result<(), String> {
                self.0.merge_rooms(r, rank).await
            }
            async fn merge_outbound_dms(
                &mut self,
                s: OutboundDmStore,
                rank: Option<u32>,
            ) -> Result<(), String> {
                self.0.merge_outbound_dms(s, rank).await
            }
            async fn flush(&mut self) -> Result<(), String> {
                self.0.flush().await
            }
        }
        let mut seam = Blind(MemorySeam::new());
        seam.0.rooms.removed_rooms.insert(d);
        block_on(top_up_from_with(&channel, &mut seam, &lineage3()[2])).unwrap();
        assert!(!seam.0.rooms.map.contains_key(&d));
    }

    /// #768 round 2 item 4: when a predecessor's own `rooms_meta` records that a
    /// room's identity came from an OLDER generation, the import carries that
    /// provenance into the rank registry, so a generation newer than the
    /// provenance (but older than the predecessor) still wins. Without it the
    /// identity is re-stamped at the predecessor's rank and the newer one is
    /// lost when this delegate itself becomes a predecessor.
    #[test]
    fn an_imported_rooms_meta_carries_its_identity_provenance() {
        let room = vk(5);
        // gen2 holds the room with gen0's identity (50), and says so.
        let mut gen2 = DelegateSim::default();
        gen2.store.insert(
            chat_delegate::room_storage_key(&room),
            cbor(&RoomSlot::Present(Box::new(room_with_identity(room, 50)))),
        );
        gen2.store.insert(
            ROOMS_META_KEY.to_vec(),
            cbor(&RoomsMeta {
                current_room_key: None,
                notification_modes: HashMap::new(),
                room_order: vec![room],
                identity_sources: vec![(room, table3()[0].0)],
            }),
        );
        // gen1 holds the room with a NEWER identity (51) than gen0's.
        let mut gen1 = DelegateSim::default();
        gen1.store.insert(
            chat_delegate::room_storage_key(&room),
            cbor(&RoomSlot::Present(Box::new(room_with_identity(room, 51)))),
        );
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), DelegateSim::default())
            .with_delegate(&pred_key(1), gen1)
            .with_delegate(&pred_key(2), gen2)
            .with_delegate(&successor_key(), DelegateSim::default());
        let (report, seam) = walk(&channel, MemorySeam::new());
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(
            seam.rooms.map[&room].self_verifying_key().unwrap(),
            SigningKey::from_bytes(&[51; 32]).verifying_key(),
            "gen1's identity is newer than the gen0 identity gen2 carried"
        );
        // The lowering precedes the save that persists the lowered ranks.
        let lower = seam.events.iter().position(|e| *e == "lower");
        assert!(lower.is_some(), "premise: the provenance was lowered");
        assert_eq!(
            seam.events.get(lower.unwrap() + 1),
            Some(&"flush"),
            "lowered, then flushed: {:?}",
            seam.events
        );
    }
}
