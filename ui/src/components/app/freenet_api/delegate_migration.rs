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
use crate::room_data::{MergeAuthority, MergeRanks, Rooms, RoomSlot, RoomsMeta};

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
        .map(|(generation, (delegate_key, code_hash))| DelegateLineageEntry {
            generation: generation as u32,
            code_hash: *code_hash,
            delegate_key: *delegate_key,
            // True for rows whose recorded key does not derive from the code
            // hash (River's V1). The walk never re-derives keys, so this is
            // metadata — but `predecessor_delegate_keys_checked` would assert
            // on it, so record it truthfully rather than hardcoding false.
            irregular_key: blake3::hash(code_hash).as_bytes() != delegate_key,
            note: "river legacy delegate",
        })
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
        let probes = predecessors.iter().map(|entry| {
            let key = DelegateKey::new(entry.delegate_key, CodeHash::new(entry.code_hash));
            async move {
                let outcome = self
                    .channel
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
    async fn key_index(&mut self, predecessor: &DelegateKey) -> Result<Vec<ChatDelegateKey>, String> {
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
                    Some(ChatDelegateResponseMsg::ListResponse { keys }) => {
                        Prewarmed::Listed(keys)
                    }
                    Some(_) => Prewarmed::Executed,
                    None => Prewarmed::Silent,
                };
                let executable = learned != Prewarmed::Silent;
                self.prewarmed
                    .insert(predecessor.bytes().to_vec(), learned);
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
            match self.get_key(predecessor, &storage_key).await? {
                Some(value) => pairs.push((storage_key, value)),
                // Definitive absence — the delegate executed and said "no
                // value". A legitimate skip, mirroring the sweep's
                // `GetResponse { value: None }` arm.
                None => {}
            }
        }
        Ok(pairs)
    }
}

// ---------------------------------------------------------------------------
// Successor side
// ---------------------------------------------------------------------------

/// What one recovered pair decodes to. Pure classification, split from the
/// I/O so the mapping is unit-testable.
#[derive(Debug)]
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
            let resurrected: Vec<[u8; 32]> =
                chat_delegate::with_identity_source_ranks(|ranks| {
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
        // Same helpers the sweep's legacy DM arm uses; both defer their signal
        // writes internally and are idempotent per entry.
        chat_delegate::hydrate_hidden_dm_threads(store.hidden_threads);
        chat_delegate::hydrate_outbound_dms_cache(store.entries);
        self.dms_dirty = true;
        Ok(())
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
            Some(other) => Err(format!("unexpected reply to marker StoreRequest: {other:?}")),
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
        if let Some(value) = self.get_marker(pred_done_marker_key(query.predecessor)).await? {
            return Ok(Some(MigrationMarker::Done {
                had_data: value != PRED_DONE_MARKER_VALUE_EMPTY,
            }));
        }
        match self.get_marker(pred_wip_marker_key(query.predecessor)).await? {
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
            MigrationMarker::InProgress { saw_data } => (pred_wip_marker_key(predecessor), saw_data),
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
    let mut reader = RiverPredecessorIo::new(&channel);
    reader.prewarm(&lineage).await;
    let mut writer = RiverSuccessorIo::new(
        &channel,
        LiveImportSeam::new(),
        chat_delegate::current_delegate_key(),
    );
    let report = freenet_migrate::migrate_delegate_secrets(
        &mut writer,
        &mut reader,
        &lineage,
        MigrationAuthorization::app_author_ack(),
        river_secret_policy(),
    )
    .await;

    log_migration_report(&report);
    report
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
