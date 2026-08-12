//! Delegate secret migration on `freenet-migrate` 0.5 (freenet/river#398 phase 3).
//!
//! A delegate's address is `BLAKE3(BLAKE3(wasm) ‖ params)`, so any rebuild
//! re-keys it and the successor starts with an EMPTY secret store. River's
//! delegate holds each room's identity key (`RoomData::self_sk`), which cannot
//! be re-derived from the network — losing it loses the user's identity in that
//! room. River has hand-rolled the recovery sweep in `chat_delegate.rs` /
//! `response_handler.rs` for 27 generations; this module runs the shared,
//! reviewed library's walk ALONGSIDE that sweep (release 1, the shape Delta
//! shipped), so the walk order, marker bookkeeping, withholding and
//! per-predecessor classification come from one place. Retiring the hand-rolled
//! sweep is release 2, after field validation — nothing here removes it.
//!
//! # This is a UI-side adoption — the delegate WASM does NOT change
//!
//! The delegate WASM is byte-identical before and after this module was added
//! (pinned by `chat_delegate_wasm_is_byte_identical` in `chat_delegate.rs`), and
//! that is load-bearing: changing it would re-key the delegate and create
//! exactly the data-loss event the module exists to prevent. It is possible
//! because River's delegate protocol already has both halves of the writer
//! seam: `GetRequest`/`ListRequest` read a predecessor, and the current
//! delegate's Store/CAS paths (via `save_rooms_to_delegate`'s per-room CAS
//! `reconcile_room_present`) are the app's own import path.
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
//! 2. **Never trust the legacy List alone.** The legacy WASM's `get_key_index`
//!    swallows decode errors into an empty list
//!    (`delegates/chat-delegate/src/handlers.rs`, frozen — cannot be fixed), so
//!    an empty/short index is not proof of absence. [`fetch_secrets`] unions
//!    the listed keys with the FIXED probes `rooms_data` + `outbound_dms`, so
//!    the single-blob era is recovered even through a corrupt index. (Per-room
//!    `room:<vk>` keys are dynamic and unguessable, so a corrupt index still
//!    strands those — the union is a floor, not a fix for the frozen WASM.)
//! 3. **Never invent a second merge.** The writer routes every recovered room
//!    through `Rooms::merge_from_source` at the generation's source rank with
//!    the SHARED identity/tombstone rank registry — the same machinery the
//!    sweep hydrates through — so ranked tombstones (freenet/river#590) and
//!    identity conflicts (freenet/river#527) are handled identically, and the
//!    flush persists through `save_rooms_to_delegate`'s per-room CAS
//!    (`reconcile_room_present`). This is stronger than the never-clobber the
//!    crate's Union policy rests on: precedence comes from generation RANK,
//!    not arrival order.
//! 4. **Markers are durable, in the CURRENT delegate's KV store.** ghostkeys
//!    deliberately went the opposite way (in-memory, page-lifetime) because
//!    sealing a predecessor there can strand late-arriving data. River's case
//!    is different: a legacy delegate's data is FROZEN after the re-key (only
//!    the current delegate is ever written), so sealing is safe — and durable
//!    markers are what stop the walk re-running on every load in the gateway
//!    iframe, where `localStorage` is unavailable and the sweep's own
//!    `is_legacy_migration_done` flag cannot persist. Do not copy either
//!    choice into another app without re-deriving which case applies.
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

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};

use dioxus::logger::tracing::{info, warn};
use freenet_migrate::{
    DelegateLineageEntry, DelegateMigrationReport, ItemWrite, MarkerQuery, MigrationAuthorization,
    MigrationMarker, PredecessorSecretsIo, RecoveredSecret, SecretPair, SecretSelectionPolicy,
    SuccessorSecretsIo, UnionAck, PRED_DONE_MARKER_VALUE_DATA, PRED_DONE_MARKER_VALUE_EMPTY,
};
use freenet_stdlib::prelude::{CodeHash, DelegateKey};
use river_core::chat_delegate::{
    ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg, OutboundDmStore,
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
/// why [`run_crate_delegate_migration`] waits for the load to settle first —
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
}

impl<'a, C: RiverDelegateChannel> RiverPredecessorIo<'a, C> {
    pub(crate) fn new(channel: &'a C) -> Self {
        Self {
            channel,
            prewarmed: HashMap::new(),
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
        let probes = predecessors.iter().map(|entry| {
            let key = DelegateKey::new(entry.delegate_key, CodeHash::new(entry.code_hash));
            async move {
                let outcome = channel
                    .request(&key, ChatDelegateRequestMsg::ListRequest)
                    .await;
                (key.bytes().to_vec(), outcome)
            }
        });
        for (key_bytes, outcome) in futures::future::join_all(probes).await {
            let learned = match outcome {
                Ok(Some(ChatDelegateResponseMsg::ListResponse { keys })) => Prewarmed::Listed(keys),
                Ok(Some(_)) => Prewarmed::Executed,
                Ok(None) => Prewarmed::Silent,
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
/// index: the listed keys UNIONED with the fixed probes, minus this module's
/// own markers, ordered so `rooms_meta` comes LAST (its `room_order` merge
/// only retains rooms already present, so the rooms must merge first).
///
/// Pure, so the union rule — the "never trust List alone" constraint — is
/// unit-testable and mutation-testable on its own.
pub(crate) fn candidate_keys(listed: &[ChatDelegateKey]) -> Vec<Vec<u8>> {
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut meta = false;
    for k in listed {
        let b = k.as_bytes();
        if is_migration_marker_key(b) {
            continue;
        }
        if b == ROOMS_META_KEY {
            meta = true;
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
    if meta {
        keys.push(ROOMS_META_KEY.to_vec());
    }
    keys
}

impl<C: RiverDelegateChannel> PredecessorSecretsIo for RiverPredecessorIo<'_, C> {
    type Error = String;

    /// Executability preflight, answered from the pre-warm cache when
    /// available. Any reply counts as executed; only silence is `Ok(false)`.
    async fn probe_executable(&mut self, predecessor: &DelegateKey) -> Result<bool, Self::Error> {
        match self.prewarmed.get(predecessor.bytes()) {
            Some(Prewarmed::Listed(_)) | Some(Prewarmed::Executed) => Ok(true),
            Some(Prewarmed::Silent) => Ok(false),
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
                    None => Prewarmed::Silent,
                };
                let executable = learned != Prewarmed::Silent;
                self.prewarmed.insert(predecessor.bytes().to_vec(), learned);
                Ok(executable)
            }
        }
    }

    /// Enumerate `predecessor`'s secrets: key index ∪ fixed probes, each key
    /// GET'd individually. A key the delegate DEFINITIVELY reports absent
    /// (`value: None`) is skipped; any unknown outcome aborts the predecessor
    /// (`Err` → `Unresponsive`) rather than under-reporting its data.
    async fn fetch_secrets(
        &mut self,
        predecessor: &DelegateKey,
    ) -> Result<Vec<SecretPair>, Self::Error> {
        let listed = self.key_index(predecessor).await?;
        let mut pairs: Vec<SecretPair> = Vec::new();
        for storage_key in candidate_keys(&listed) {
            // A `None` here is definitive absence — the delegate executed and
            // said "no value" — a legitimate skip, mirroring the sweep's
            // `GetResponse { value: None }` arm. Anything unknown already
            // aborted via the `?`.
            if let Some(value) = self.get_key(predecessor, &storage_key).await? {
                pairs.push((storage_key, value));
            }
        }
        Ok(pairs)
    }
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
    /// One of this module's own migration markers (a future predecessor's
    /// store carries them) — never imported, never a failure.
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
    if is_migration_marker_key(key) {
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

    /// Merge a recovered outbound-DM store into the local cache.
    fn merge_outbound_dms(
        &mut self,
        store: OutboundDmStore,
    ) -> impl Future<Output = Result<(), String>>;

    /// Persist everything merged so far; `Ok` only when the save REALLY
    /// landed. This is the durability boundary the crate's flush contract
    /// exists for — a failure here re-counts this predecessor's optimistic
    /// writes as failures and withholds its keys from older generations.
    fn flush(&mut self) -> impl Future<Output = Result<(), String>>;
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
}

impl LiveImportSeam {
    pub(crate) fn new() -> Self {
        Self {
            rooms_dirty: false,
            dms_dirty: false,
        }
    }
}

impl RiverImportSeam for LiveImportSeam {
    async fn merge_rooms(&mut self, rooms: Rooms, source_rank: u32) -> Result<(), String> {
        self.rooms_dirty = true;
        let (tx, rx) = futures::channel::oneshot::channel::<Result<(), String>>();
        crate::util::defer(move || {
            let result = crate::components::app::ROOMS.with_mut(|current| {
                let merged = chat_delegate::with_identity_source_ranks(|ranks| {
                    apply_recovered_rooms(current, rooms, source_rank, ranks)
                });
                // Re-decrypt secrets for merged rooms (they are #[serde(skip)]),
                // as the sweep's hydrate does. Idempotent and cheap.
                for room_data in current.map.values_mut() {
                    room_data.repopulate_secrets_from_state();
                }
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

    async fn merge_outbound_dms(&mut self, store: OutboundDmStore) -> Result<(), String> {
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
        let (tx, rx) = futures::channel::oneshot::channel::<()>();
        crate::util::defer(move || {
            chat_delegate::hydrate_hidden_dm_threads_now(store.hidden_threads);
            chat_delegate::hydrate_outbound_dms_cache_now(store.entries);
            let _ = tx.send(());
        });
        rx.await
            .map_err(|_| "outbound-DM merge was dropped before running".to_string())
    }

    async fn flush(&mut self) -> Result<(), String> {
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
}

impl<'a, C: RiverDelegateChannel, S: RiverImportSeam> RiverSuccessorIo<'a, C, S> {
    pub(crate) fn new(channel: &'a C, seam: S, successor: DelegateKey) -> Self {
        Self {
            channel,
            seam,
            successor,
        }
    }

    /// GET a marker key from the CURRENT delegate. Three-way: value / definitive
    /// absence / `Err` for silence, transport failure, or an unexpected reply.
    /// An unreadable marker MUST be `Err` (→ `WriterUnavailable`, walk stops):
    /// guessing `None` would re-import a possibly-done predecessor, and
    /// guessing `Done` would silently skip one.
    async fn get_marker(&mut self, storage_key: Vec<u8>) -> Result<Option<Vec<u8>>, String> {
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

    /// Store a marker on the CURRENT delegate, awaiting the delegate's ack so
    /// the marker is durable when this returns (the crate's record_marker
    /// contract — markers are never batched with items).
    async fn put_marker(&mut self, storage_key: Vec<u8>, value: &[u8]) -> Result<(), String> {
        let request = ChatDelegateRequestMsg::StoreRequest {
            key: ChatDelegateKey::new(storage_key),
            value: value.to_vec(),
        };
        match self.channel.request(&self.successor, request).await? {
            Some(ChatDelegateResponseMsg::StoreResponse { result, .. }) => {
                result.map_err(|e| format!("delegate refused marker store: {e}"))
            }
            Some(other) => Err(format!(
                "unexpected reply to marker StoreRequest: {other:?}"
            )),
            None => Err("marker StoreRequest timed out".to_string()),
        }
    }
}

impl<C: RiverDelegateChannel, S: RiverImportSeam> SuccessorSecretsIo
    for RiverSuccessorIo<'_, C, S>
{
    type Error = String;

    async fn migration_marker(
        &mut self,
        query: &MarkerQuery<'_>,
    ) -> Result<Option<MigrationMarker>, Self::Error> {
        // `query.legacy_bridge` is ignored: River never used the crate's older
        // generation-keyed `import_secrets_once` markers (module docs).
        if let Some(value) = self
            .get_marker(pred_done_marker_key(query.predecessor))
            .await?
        {
            return Ok(Some(MigrationMarker::Done {
                had_data: value != PRED_DONE_MARKER_VALUE_EMPTY,
            }));
        }
        match self
            .get_marker(pred_wip_marker_key(query.predecessor))
            .await?
        {
            Some(value) => Ok(Some(MigrationMarker::InProgress {
                saw_data: value != PRED_DONE_MARKER_VALUE_EMPTY,
            })),
            None => Ok(None),
        }
    }

    async fn record_marker(
        &mut self,
        predecessor: &DelegateKey,
        marker: MigrationMarker,
    ) -> Result<(), Self::Error> {
        let (key, data) = match marker {
            MigrationMarker::InProgress { saw_data } => {
                (pred_wip_marker_key(predecessor), saw_data)
            }
            MigrationMarker::Done { had_data } => (pred_done_marker_key(predecessor), had_data),
        };
        let value = if data {
            PRED_DONE_MARKER_VALUE_DATA
        } else {
            PRED_DONE_MARKER_VALUE_EMPTY
        };
        self.put_marker(key, value).await
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
                match self.seam.merge_rooms(rooms, item.generation).await {
                    Ok(()) => ItemWrite::Written,
                    Err(e) => ItemWrite::retryable(e),
                }
            }
            RecoveredItem::OutboundDms(store) => match self.seam.merge_outbound_dms(*store).await {
                Ok(()) => ItemWrite::Written,
                Err(e) => ItemWrite::retryable(e),
            },
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

    async fn flush_predecessor(&mut self, _predecessor: &DelegateKey) -> Result<(), Self::Error> {
        self.seam.flush().await
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Page-load-scoped latch for the crate walk. A `static` is per-WASM-instance
/// state, and a WASM instance lives exactly one page load — so "once per
/// `static`" IS "once per page load". Deliberately NOT persisted anywhere:
/// cross-load once-ness comes from the durable per-predecessor markers, and
/// this latch only stops WebSocket reconnects within one load from stacking
/// concurrent walks (Delta ships the same latch, untested; this one is
/// exercised by `arm_walk_latch_fires_exactly_once`).
static CRATE_WALK_ARMED: AtomicBool = AtomicBool::new(false);

/// Arm the once-per-page-load crate walk. `true` exactly once per page load;
/// every later call (reconnects, retry paths) gets `false` and must not spawn
/// a second walk.
pub(crate) fn arm_crate_walk_once() -> bool {
    arm_walk_latch(&CRATE_WALK_ARMED)
}

/// Testable core of [`arm_crate_walk_once`] (the production latch is a global
/// `static`, which a test cannot reset).
fn arm_walk_latch(latch: &AtomicBool) -> bool {
    !latch.swap(true, Ordering::Relaxed)
}

/// How long the walk will wait for the initial load (the hand-rolled sweep
/// included) to settle before starting, so its round-trips don't contend with
/// the sweep's single-waiter correlation slots. Bounded: after this it starts
/// anyway — a collision degrades to `Unresponsive` + retry-next-load, never to
/// a wrong write.
const WALK_QUIESCENCE_MAX_MS: u64 = 60_000;
const WALK_QUIESCENCE_POLL_MS: u64 = 500;

/// Run the crate's delegate-secret migration walk once.
///
/// Spawned from `fire_legacy_migration_request` behind the once-per-page-load
/// latch (`arm_crate_walk_once`), so it inherits the freenet/river#253 gate:
/// it only ever starts when the current delegate was observed empty or a prior
/// migration was interrupted. Runs ALONGSIDE the hand-rolled sweep (release 1);
/// its durable per-predecessor markers make subsequent page loads cheap
/// (marker reads short-circuit to `AlreadyMigrated`) even in the gateway
/// iframe, where the sweep's own localStorage latch cannot persist.
pub(crate) async fn run_crate_delegate_migration() -> DelegateMigrationReport {
    // Let the sweep's burst finish first (see `NodeDelegateChannel` on why).
    let mut waited = 0u64;
    while chat_delegate::pending_load_workers() != 0 && waited < WALK_QUIESCENCE_MAX_MS {
        crate::util::sleep(std::time::Duration::from_millis(WALK_QUIESCENCE_POLL_MS)).await;
        waited += WALK_QUIESCENCE_POLL_MS;
    }

    let lineage = river_delegate_lineage(chat_delegate::legacy_delegate_pairs());
    let channel = NodeDelegateChannel;
    let (report, _seam) = walk_with(
        &channel,
        LiveImportSeam::new(),
        chat_delegate::current_delegate_key(),
        &lineage,
    )
    .await;

    log_migration_report(&report);
    report
}

/// The transport/seam-generic core of [`run_crate_delegate_migration`]:
/// pre-warm the predecessors, then drive the crate's walk with River's reader
/// and writer adapters. Production supplies [`NodeDelegateChannel`] +
/// [`LiveImportSeam`]; the tests below supply mocks — everything in between
/// (probe semantics, the union fetch, classification, ranked merge routing,
/// marker bookkeeping) is EXACTLY the code production runs.
pub(crate) async fn walk_with<C: RiverDelegateChannel, S: RiverImportSeam>(
    channel: &C,
    seam: S,
    successor: DelegateKey,
    lineage: &[DelegateLineageEntry],
) -> (DelegateMigrationReport, S) {
    let mut reader = RiverPredecessorIo::new(channel);
    reader.prewarm(lineage).await;
    let mut writer = RiverSuccessorIo::new(channel, seam, successor);
    let report = freenet_migrate::migrate_delegate_secrets(
        &mut writer,
        &mut reader,
        lineage,
        MigrationAuthorization::app_author_ack(),
        river_secret_policy(),
    )
    .await;
    (report, writer.seam)
}

/// Log the walk's outcome. The report is METADATA ONLY and is never rendered
/// to the user as counts (`imported_total` legitimately reads 0 for a fully
/// successful recovery — freenet-migrate#16); the freenet/river#204 gate
/// (`any_unresponsive`) is surfaced as a warning so a stranded generation is
/// visible in diagnostics. The hand-rolled sweep owns the user-facing load
/// states in release 1.
fn log_migration_report(report: &DelegateMigrationReport) {
    for p in &report.predecessors {
        info!(
            "crate-migration predecessor gen {}: {:?}",
            p.generation(),
            p
        );
    }
    if report.any_unresponsive() {
        warn!(
            "crate-migration: {} predecessor generation(s) could not be confirmed executable — \
             their data (if any) may exist but cannot auto-migrate; the walk retries next load",
            report.unresponsive().count()
        );
    }
    if report.is_complete() {
        info!("crate-migration: walk complete");
    } else if report.retry_may_help() {
        info!("crate-migration: walk incomplete; a later run may make progress");
    } else {
        warn!("crate-migration: walk incomplete with non-retryable rejections");
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
    use super::*;
    use ed25519_dalek::{SigningKey, VerifyingKey};
    use freenet_migrate::{PredecessorMigration, WriterFailure, WriterStage};
    use futures::executor::block_on;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::pin::Pin;
    use std::task::{Context, Poll};

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

    fn successor_key() -> DelegateKey {
        DelegateKey::new([0xAA; 32], CodeHash::new([0xAB; 32]))
    }

    // ---------------- scripted transport ----------------

    /// Yields once (waking itself) so concurrent mock requests genuinely
    /// overlap under `join_all`, making `max_in_flight` a real measurement.
    struct YieldOnce(bool);
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
    struct DelegateSim {
        /// KV store; GETs answer from it, Stores land in it.
        store: BTreeMap<Vec<u8>, Vec<u8>>,
        /// Override what `ListRequest` reports (None = the store's keys).
        /// `Some(vec![])` models the frozen legacy WASM's corrupt-index
        /// behaviour: an empty list even though the store holds data.
        listed: Option<Vec<Vec<u8>>>,
        /// The whole delegate never answers (not installed / cannot execute).
        silent: bool,
        /// GETs for these keys never answer (per-key silence).
        silent_get_keys: Vec<Vec<u8>>,
        /// When a GET asks for `.0`, reply names key `.1` with value `.2` —
        /// a mis-correlated answer.
        wrong_key_reply: Option<(Vec<u8>, Vec<u8>, Vec<u8>)>,
    }

    #[derive(Default)]
    struct MockChannel {
        delegates: RefCell<HashMap<Vec<u8>, DelegateSim>>,
        in_flight: Cell<usize>,
        max_in_flight: Cell<usize>,
        /// `(target delegate key bytes, "list" | "get:<key>" | "store:<key>")`
        log: RefCell<Vec<(Vec<u8>, String)>>,
    }

    impl MockChannel {
        fn with_delegate(self, key: &DelegateKey, sim: DelegateSim) -> Self {
            self.delegates
                .borrow_mut()
                .insert(key.bytes().to_vec(), sim);
            self
        }
        fn reset_counters(&self) {
            self.in_flight.set(0);
            self.max_in_flight.set(0);
            self.log.borrow_mut().clear();
        }
        fn stored(&self, delegate: &DelegateKey, key: &[u8]) -> Option<Vec<u8>> {
            self.delegates
                .borrow()
                .get(delegate.bytes())
                .and_then(|sim| sim.store.get(key).cloned())
        }
        fn requests_to_predecessors(&self, kind: &str) -> usize {
            let succ = successor_key().bytes().to_vec();
            self.log
                .borrow()
                .iter()
                .filter(|(target, what)| *target != succ && what.starts_with(kind))
                .count()
        }
        fn answer(
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
            match request {
                ChatDelegateRequestMsg::ListRequest => {
                    self.log
                        .borrow_mut()
                        .push((target.bytes().to_vec(), "list".into()));
                    if sim.silent {
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
                    if sim.silent || sim.silent_get_keys.iter().any(|k| *k == kb) {
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
                    sim.store.insert(kb, value);
                    Ok(Some(ChatDelegateResponseMsg::StoreResponse {
                        key,
                        value_size,
                        result: Ok(()),
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
            self.in_flight.set(self.in_flight.get() + 1);
            self.max_in_flight
                .set(self.max_in_flight.get().max(self.in_flight.get()));
            YieldOnce(false).await;
            let out = self.answer(target, request);
            self.in_flight.set(self.in_flight.get() - 1);
            out
        }
    }

    // ---------------- in-memory seam ----------------

    /// In-memory [`RiverImportSeam`] holding a REAL `Rooms` + `MergeRanks`, so
    /// merges route through the production `Rooms::merge_from_source` — the
    /// #590/#527 machinery itself, not a reimplementation.
    struct MemorySeam {
        rooms: Rooms,
        ranks: MergeRanks,
        dm_merges: Vec<OutboundDmStore>,
        flushes: usize,
        fail_flush: bool,
    }

    impl MemorySeam {
        fn new() -> Self {
            Self {
                rooms: empty_rooms(),
                ranks: MergeRanks::default(),
                dm_merges: Vec::new(),
                flushes: 0,
                fail_flush: false,
            }
        }
    }

    impl RiverImportSeam for MemorySeam {
        async fn merge_rooms(&mut self, rooms: Rooms, source_rank: u32) -> Result<(), String> {
            apply_recovered_rooms(&mut self.rooms, rooms, source_rank, &mut self.ranks)
        }
        async fn merge_outbound_dms(&mut self, store: OutboundDmStore) -> Result<(), String> {
            self.dm_merges.push(store);
            Ok(())
        }
        async fn flush(&mut self) -> Result<(), String> {
            self.flushes += 1;
            if self.fail_flush {
                Err("coalesced save failed".to_string())
            } else {
                Ok(())
            }
        }
    }

    fn walk(channel: &MockChannel, seam: MemorySeam) -> (DelegateMigrationReport, MemorySeam) {
        block_on(walk_with(channel, seam, successor_key(), &lineage3()))
    }

    // ---------------- the once-per-page-load latch ----------------

    /// The latch arms exactly once. Mutating `arm_walk_latch` to always
    /// return `true` (removing the latch) makes every reconnect spawn a
    /// concurrent walk — this is the test that goes red.
    #[test]
    fn arm_walk_latch_fires_exactly_once() {
        let latch = AtomicBool::new(false);
        assert!(arm_walk_latch(&latch), "first arm must fire");
        assert!(!arm_walk_latch(&latch), "second arm must NOT fire");
        assert!(!arm_walk_latch(&latch), "later arms must NOT fire");
    }

    /// Source pin: the production spawn is gated on the latch, inside
    /// `fire_legacy_migration_request` AFTER its #253-inherited done-gate and
    /// per-session guard — deleting the latch call or hoisting the spawn above
    /// the guards fails here.
    #[test]
    fn crate_walk_is_wired_behind_the_latch_and_the_253_gate() {
        let src = include_str!("../chat_delegate.rs");
        assert_eq!(
            src.matches("run_crate_delegate_migration").count(),
            1,
            "exactly one call site for the walk in chat_delegate.rs"
        );
        // rfind: the signature also appears as a string literal inside
        // chat_delegate.rs's own mid-file test module; the real definition is
        // the last fn in that file.
        let fn_start = src
            .rfind("pub(crate) async fn fire_legacy_migration_request() -> bool {")
            .expect("fire_legacy_migration_request must exist");
        let body = &src[fn_start..];
        let done_gate = body
            .find("is_legacy_migration_done()")
            .expect("the #253-inherited done-gate must exist");
        let session_guard = body
            .find("LEGACY_MIGRATION_ATTEMPTED.swap(true")
            .expect("the per-session guard must exist");
        let latch = body
            .find("if super::freenet_api::delegate_migration::arm_crate_walk_once() {")
            .expect("the spawn must be gated on arm_crate_walk_once()");
        let spawn = body
            .find("run_crate_delegate_migration")
            .expect("the walk must be spawned in this function");
        assert!(
            done_gate < latch && session_guard < latch,
            "the latch check must come after BOTH guards (inheriting the #253 gate)"
        );
        assert!(
            latch < spawn,
            "the spawn must be inside the latch-gated block"
        );
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
    /// lists nothing; markers are excluded; `rooms_meta` goes last (its
    /// `room_order` merge only keeps rooms already present).
    #[test]
    fn candidate_keys_union_fixed_probes_exclude_markers_meta_last() {
        // Empty index still probes the single-blob era's keys.
        assert_eq!(
            candidate_keys(&[]),
            vec![
                ROOMS_STORAGE_KEY.to_vec(),
                OUTBOUND_DMS_STORAGE_KEY.to_vec()
            ],
            "an empty legacy index must STILL probe rooms_data + outbound_dms"
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
                b"room:abc".to_vec(),
                ROOMS_STORAGE_KEY.to_vec(),
                OUTBOUND_DMS_STORAGE_KEY.to_vec(),
                ROOMS_META_KEY.to_vec(),
            ],
            "dedup, no markers, fixed probes unioned, rooms_meta last"
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

    /// The corrupt-index floor: the frozen legacy WASM reports a decode
    /// failure as an EMPTY list, so the fixed probes must fire even when the
    /// index lists nothing. Removing the union in `candidate_keys` turns this
    /// red.
    #[test]
    fn a_corrupt_empty_index_still_recovers_the_single_blob_era() {
        let room_a = vk(1);
        let mut gen0 = DelegateSim::default();
        gen0.store
            .insert(ROOMS_STORAGE_KEY.to_vec(), rooms_blob_with(room_a, 3));
        gen0.listed = Some(vec![]); // corrupt index: data present, list empty
        let channel = MockChannel::default()
            .with_delegate(&pred_key(0), gen0)
            .with_delegate(&pred_key(1), DelegateSim::default())
            .with_delegate(&pred_key(2), DelegateSim::default())
            .with_delegate(&successor_key(), DelegateSim::default());

        let (report, seam) = walk(&channel, MemorySeam::new());
        assert!(report.is_complete());
        assert!(
            seam.rooms.map.contains_key(&room_a),
            "rooms_data must be recovered through a corrupt (empty) index"
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
        // Use the REAL base the pre-warm's `ListRequest` registers under, not a
        // stand-in: this assertion previously ran against `b"list"`, a literal
        // that appears nowhere in the code, so it could not have noticed that
        // the real base was being completed unscoped on the response side (B1).
        let base = chat_delegate::list_request_correlation_key();
        let k0 = chat_delegate::legacy_scoped_correlation(&pred_key(0), &base);
        let k1 = chat_delegate::legacy_scoped_correlation(&pred_key(1), &base);
        let k2 = chat_delegate::legacy_scoped_correlation(&pred_key(2), &base);
        assert!(k0 != k1 && k1 != k2 && k0 != k2);
    }
}
