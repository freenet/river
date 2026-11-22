//! Shrinks the copies of the user's rooms that superseded chat-delegate
//! generations keep after their data has been migrated.
//!
//! # Why
//!
//! A delegate re-key never deletes anything: every legacy generation keeps its
//! own copy of every room it ever held. A long-time user ends up with the same
//! ~1 MB room snapshot in eight or more generations. On a hosted node the
//! secrets of one user share one quota (16 MiB on try.freenet.org), so those
//! duplicates fill it, every write to the CURRENT delegate is refused ("per-user
//! secret quota exceeded"), the migration journal keeps reporting an interrupted
//! import, and every load re-runs the full legacy sweep without ever finishing.
//!
//! # What a pass does
//!
//! Generations are visited newest first. For each room copy a legacy generation
//! holds, one of three things happens ([`decide_present_copy`]):
//!
//! * **Compact**: the copy is rewritten without its `room_state` (the room
//!   contract's state: messages, members, configuration). That is the bulk of
//!   the bytes and the one part [`chat_delegate::critical_hash`] counts as a
//!   reconstructible cache. Everything unrecoverable (`self_sk`, the invite
//!   chain, invitation secrets, own purges, nickname, ...) is kept byte for
//!   byte, and the key stays, so the generation still exports the same number
//!   of items to the migration journal. Only done while a NEWER full copy of
//!   the room is kept (in the current delegate or a newer generation), so at
//!   least one full snapshot always survives.
//! * **Delete**: the key is removed, only when ALL of these hold:
//!   - the migration journal reports the generation resolved
//!     ([`migration_journal::resolved_predecessors`]); a pending one must not
//!     shrink, or the journal stops believing its export and keeps it pending
//!     forever;
//!   - the current delegate, read in this pass, holds the room (with its full
//!     state, unless the copy is itself compacted) or a tombstone for it;
//!   - merging the copy into what the current delegate holds, with the load's
//!     own ranked merge ([`Rooms::merge_from_source`]), changes no
//!     unrecoverable field ([`redundant_under_current`]). The load would throw
//!     away whatever the copy adds, so deleting it loses nothing a load keeps.
//! * **Keep**: anything else, including every per-room tombstone (a few bytes,
//!   and an older generation's tombstone can still matter to the ranked merge).
//!
//! A legacy `rooms_data` blob is planned the same way room by room
//! ([`plan_blob`]): rewritten with the eligible rooms compacted, or deleted
//! when every room in it is deletable and its tombstones are all covered.
//!
//! # The quota-full case, and why the order cannot lose data
//!
//! With the quota full nothing can be committed to the current delegate, and
//! the journal resolves nothing, so the first pass can only compact. Compaction
//! frees the duplicates (eight copies become one full copy and seven small
//! ones) without ever needing a write to the current delegate. Then the rooms
//! are saved to the current delegate and the walk is asked to resolve the
//! journal; a second pass, once that walk is done, deletes what is resolved and
//! redundant. Interrupted at any point:
//!
//! * mid-compaction: a compacted copy keeps every unrecoverable field, and a
//!   newer full copy of the room exists, so nothing is lost;
//! * after compaction, before the save: the newest full copy is still in its
//!   legacy generation, and the next load's sweep merges it as before;
//! * mid-deletion: only copies already redundant under the current delegate's
//!   verified copy are deleted.
//!
//! # Limits
//!
//! * `outbound_dms` and `rooms_meta` are left alone: they are small, and their
//!   merges are not the room merge the redundancy check runs.
//! * The compaction is a plain `StoreRequest` (legacy generations predate or do
//!   not share the current delegate's compare-and-swap), so an old-build tab
//!   still writing to a superseded generation between this pass's read and its
//!   write would have that write replaced. Such tabs reload into the current
//!   build when they see the re-key (`generation_watch`), and the replacement
//!   keeps every unrecoverable field this pass read.
//! * Every legacy delegate WASM handles `DeleteRequest` and `StoreRequest`: both
//!   are in the protocol since the first chat delegate (March 2025), before the
//!   oldest registered generation (V1, January 2026).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ciborium::from_reader;
use dioxus::logger::tracing::{info, warn};
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::{CodeHash, DelegateKey};
use river_core::chat_delegate::{ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg};

use super::migration_journal;
use crate::components::app::chat_delegate::{
    self, critical_hash, current_delegate_source_rank, legacy_delegate_pairs,
    parse_room_storage_key, send_delegate_request, send_delegate_request_to, ROOMS_STORAGE_KEY,
};
use crate::room_data::{MergeAuthority, MergeRanks, RoomData, RoomSlot, Rooms};

// ---------------------------------------------------------------------------
// Pure decisions
// ---------------------------------------------------------------------------

/// What the current delegate holds for one room, as this pass read it.
#[derive(Clone)]
pub(crate) enum CurrentCopy {
    /// The read failed or did not decode: nothing may be concluded from it.
    Unknown,
    /// No slot.
    Absent,
    /// The user left the room.
    Tombstone,
    /// The room, as stored.
    Present(Box<RoomData>),
}

/// What to do with one legacy copy of a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyAction {
    Keep,
    Compact,
    Delete,
}

/// Whether `room` carries a room snapshot at all (a compacted or freshly
/// imported room carries the default, empty state).
pub(crate) fn has_full_state(room: &RoomData) -> bool {
    room.room_state != Default::default()
}

/// `room` without its reconstructible room snapshot. Every other field, and so
/// [`critical_hash`], is unchanged.
pub(crate) fn compact_room(room: &RoomData) -> RoomData {
    let mut compacted = room.clone();
    compacted.room_state = Default::default();
    compacted
}

fn single_room(owner: VerifyingKey, room: Option<&RoomData>) -> Rooms {
    let mut rooms = Rooms {
        map: HashMap::new(),
        current_room_key: None,
        removed_rooms: HashSet::new(),
        notification_modes: HashMap::new(),
        room_order: Vec::new(),
        migrated_rooms: Vec::new(),
    };
    match room {
        Some(r) => {
            rooms.map.insert(owner, r.clone());
        }
        None => {
            rooms.removed_rooms.insert(owner);
        }
    }
    rooms
}

/// The load's view of one room: its unrecoverable fields' hash if present, and
/// whether it is tombstoned. `None` when a present room's hash cannot be
/// computed (never treated as equal to anything).
fn room_view(rooms: &Rooms, owner: &VerifyingKey) -> Option<(Option<u64>, bool)> {
    let hash = match rooms.map.get(owner) {
        Some(room) => Some(critical_hash(room)?),
        None => None,
    };
    Some((hash, rooms.removed_rooms.contains(owner)))
}

/// Would merging `candidate` (a legacy copy at `candidate_rank`) into what the
/// current delegate holds change any unrecoverable field of the room, under the
/// load's own ranked merge? `false` (redundant) means a load keeps nothing of
/// it, so deleting it loses nothing. Any merge error, unknown current copy or
/// unhashable room answers "not redundant".
pub(crate) fn redundant_under_current(
    owner: VerifyingKey,
    current: &CurrentCopy,
    current_rank: u32,
    candidate: &RoomData,
    candidate_rank: u32,
) -> bool {
    if candidate_rank >= current_rank {
        return false;
    }
    let base = match current {
        CurrentCopy::Present(room) => single_room(owner, Some(room)),
        CurrentCopy::Tombstone => single_room(owner, None),
        CurrentCopy::Unknown | CurrentCopy::Absent => return false,
    };
    let mut rooms = single_room(owner, None);
    rooms.removed_rooms.clear();
    let mut ranks = MergeRanks::default();
    if rooms
        .merge_from_source(
            base,
            current_rank,
            MergeAuthority::Authoritative,
            &mut ranks,
        )
        .is_err()
    {
        return false;
    }
    let Some(before) = room_view(&rooms, &owner) else {
        return false;
    };
    if rooms
        .merge_from_source(
            single_room(owner, Some(candidate)),
            candidate_rank,
            MergeAuthority::OlderSnapshot,
            &mut ranks,
        )
        .is_err()
    {
        return false;
    }
    room_view(&rooms, &owner) == Some(before)
}

/// Whether the current delegate's copy is one a legacy copy may be deleted
/// under: a tombstone, or the room with its full snapshot (a compacted legacy
/// copy only needs the room).
fn current_supersedes(current: &CurrentCopy, candidate_full: bool) -> bool {
    match current {
        CurrentCopy::Tombstone => true,
        CurrentCopy::Present(room) => !candidate_full || has_full_state(room),
        CurrentCopy::Unknown | CurrentCopy::Absent => false,
    }
}

/// The inputs [`decide_present_copy`] decides one legacy copy from.
pub(crate) struct CopyContext<'a> {
    pub(crate) owner: VerifyingKey,
    /// What the current delegate holds for the room.
    pub(crate) current: &'a CurrentCopy,
    pub(crate) current_rank: u32,
    /// A full copy of the room NEWER than this one is kept: the current
    /// delegate's, or one a newer generation keeps.
    pub(crate) newer_full_copy_kept: bool,
    /// The migration journal reports this generation resolved.
    pub(crate) generation_resolved: bool,
    pub(crate) candidate_rank: u32,
}

/// Decide one legacy `Present` copy. See the module docs for the rules.
pub(crate) fn decide_present_copy(ctx: &CopyContext<'_>, candidate: &RoomData) -> CopyAction {
    let full = has_full_state(candidate);
    if ctx.generation_resolved
        && current_supersedes(ctx.current, full)
        && redundant_under_current(
            ctx.owner,
            ctx.current,
            ctx.current_rank,
            candidate,
            ctx.candidate_rank,
        )
    {
        return CopyAction::Delete;
    }
    let tombstoned = matches!(ctx.current, CurrentCopy::Tombstone);
    if full && (ctx.newer_full_copy_kept || tombstoned) {
        return CopyAction::Compact;
    }
    CopyAction::Keep
}

/// What to do with a legacy `rooms_data` blob.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BlobAction {
    Keep,
    /// Rewrite it with these bytes (the eligible rooms compacted).
    Rewrite(Vec<u8>),
    Delete,
}

/// Plan a legacy generation's `rooms_data` blob room by room, as
/// [`decide_present_copy`] would each room. Deleted only when every room in it
/// is deletable and each of its tombstones is for a room the current delegate
/// holds (whose authoritative copy outranks the tombstone either way). A room
/// that could be deleted but sits in a blob that must stay is compacted
/// instead. Returns the action and the rooms it keeps in full.
pub(crate) fn plan_blob(
    blob: &Rooms,
    current: &HashMap<[u8; 32], CurrentCopy>,
    newer_full_kept: &HashSet<[u8; 32]>,
    current_rank: u32,
    generation_resolved: bool,
    candidate_rank: u32,
) -> (BlobAction, Vec<VerifyingKey>) {
    let unknown = CurrentCopy::Unknown;
    let mut kept_full = Vec::new();
    let mut compact = Vec::new();
    let mut all_deletable = true;
    for (owner, room) in &blob.map {
        let cur = current.get(&owner.to_bytes()).unwrap_or(&unknown);
        let action = decide_present_copy(
            &CopyContext {
                owner: *owner,
                current: cur,
                current_rank,
                newer_full_copy_kept: newer_full_kept.contains(&owner.to_bytes()),
                generation_resolved,
                candidate_rank,
            },
            room,
        );
        match action {
            CopyAction::Delete => {
                if has_full_state(room) {
                    compact.push(*owner);
                }
            }
            CopyAction::Compact => {
                all_deletable = false;
                compact.push(*owner);
            }
            CopyAction::Keep => {
                all_deletable = false;
                if has_full_state(room) {
                    kept_full.push(*owner);
                }
            }
        }
    }
    let tombstones_covered = blob.removed_rooms.iter().all(|vk| {
        matches!(
            current.get(&vk.to_bytes()),
            Some(CurrentCopy::Tombstone | CurrentCopy::Present(_))
        )
    });
    if generation_resolved && all_deletable && tombstones_covered {
        return (BlobAction::Delete, kept_full);
    }
    if compact.is_empty() {
        return (BlobAction::Keep, kept_full);
    }
    let mut rewritten = blob.clone();
    for owner in &compact {
        if let Some(room) = rewritten.map.get_mut(owner) {
            room.room_state = Default::default();
        }
    }
    let mut bytes = Vec::new();
    match ciborium::ser::into_writer(&rewritten, &mut bytes) {
        Ok(()) => (BlobAction::Rewrite(bytes), kept_full),
        Err(_) => (BlobAction::Keep, kept_full),
    }
}

/// Encode a compacted `Present` slot for `room`.
fn compacted_slot_bytes(room: &RoomData) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&RoomSlot::Present(Box::new(compact_room(room))), &mut bytes)
        .ok()?;
    Some(bytes)
}

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

/// What one pass changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PassReport {
    pub(crate) compacted: u32,
    pub(crate) deleted: u32,
    pub(crate) failed: u32,
    /// Bytes no longer stored, as far as this pass can tell (value sizes).
    pub(crate) bytes_freed: u64,
}

impl PassReport {
    fn changed_anything(&self) -> bool {
        self.compacted > 0 || self.deleted > 0
    }
}

async fn read_current_copy(owner: &VerifyingKey) -> CurrentCopy {
    let request = ChatDelegateRequestMsg::GetRequest {
        key: ChatDelegateKey::new(chat_delegate::room_storage_key(owner)),
    };
    match send_delegate_request(request).await {
        Ok(ChatDelegateResponseMsg::GetResponse { value: None, .. }) => CurrentCopy::Absent,
        Ok(ChatDelegateResponseMsg::GetResponse {
            value: Some(bytes), ..
        }) => match from_reader::<RoomSlot, _>(&bytes[..]) {
            Ok(RoomSlot::Present(room)) => CurrentCopy::Present(room),
            Ok(RoomSlot::Tombstone) => CurrentCopy::Tombstone,
            Err(_) => CurrentCopy::Unknown,
        },
        _ => CurrentCopy::Unknown,
    }
}

async fn current_copy<'a>(
    cache: &'a mut HashMap<[u8; 32], CurrentCopy>,
    owner: &VerifyingKey,
) -> &'a CurrentCopy {
    let key = owner.to_bytes();
    if let std::collections::hash_map::Entry::Vacant(slot) = cache.entry(key) {
        slot.insert(read_current_copy(owner).await);
    }
    &cache[&key]
}

async fn legacy_get(delegate: &DelegateKey, key: &[u8]) -> Option<Vec<u8>> {
    let request = ChatDelegateRequestMsg::GetRequest {
        key: ChatDelegateKey::new(key.to_vec()),
    };
    match send_delegate_request_to(delegate.clone(), request).await {
        Ok(ChatDelegateResponseMsg::GetResponse { value, .. }) => value,
        _ => None,
    }
}

async fn legacy_store(delegate: &DelegateKey, key: &[u8], value: Vec<u8>) -> bool {
    let request = ChatDelegateRequestMsg::StoreRequest {
        key: ChatDelegateKey::new(key.to_vec()),
        value,
    };
    matches!(
        send_delegate_request_to(delegate.clone(), request).await,
        Ok(ChatDelegateResponseMsg::StoreResponse { result: Ok(()), .. })
    )
}

async fn legacy_delete(delegate: &DelegateKey, key: &[u8]) -> bool {
    let request = ChatDelegateRequestMsg::DeleteRequest {
        key: ChatDelegateKey::new(key.to_vec()),
    };
    matches!(
        send_delegate_request_to(delegate.clone(), request).await,
        Ok(ChatDelegateResponseMsg::DeleteResponse { result: Ok(()), .. })
    )
}

fn record(report: &mut PassReport, action: CopyAction, ok: bool, freed: u64) {
    match (action, ok) {
        (_, false) => report.failed += 1,
        (CopyAction::Compact, true) => {
            report.compacted += 1;
            report.bytes_freed += freed;
        }
        (CopyAction::Delete, true) => {
            report.deleted += 1;
            report.bytes_freed += freed;
        }
        (CopyAction::Keep, true) => {}
    }
}

/// One pass over every legacy generation, newest first. The caller holds the
/// journal lock and the legacy slot-fetch lock.
async fn run_pass(resolved: Option<HashSet<[u8; 32]>>) -> PassReport {
    let current_rank = current_delegate_source_rank();
    let mut report = PassReport::default();
    let mut current: HashMap<[u8; 32], CurrentCopy> = HashMap::new();
    // Rooms a kept full copy of exists, newer than the generation being
    // visited. Seeded per room from the current delegate when it is read.
    let mut full_kept: HashSet<[u8; 32]> = HashSet::new();
    for (index, (key_bytes, code_hash)) in legacy_delegate_pairs().iter().enumerate().rev() {
        let delegate = DelegateKey::new(*key_bytes, CodeHash::new(*code_hash));
        let keys =
            match send_delegate_request_to(delegate.clone(), ChatDelegateRequestMsg::ListRequest)
                .await
            {
                Ok(ChatDelegateResponseMsg::ListResponse { keys }) => keys,
                // Silence or an error concludes nothing: skip this generation.
                _ => continue,
            };
        let rank = index as u32;
        let resolved_here = resolved.as_ref().is_some_and(|r| r.contains(key_bytes));
        let mut seen = HashSet::new();
        let mut room_keys = Vec::new();
        let mut has_blob = false;
        for key in keys {
            let bytes = key.as_bytes().to_vec();
            if !seen.insert(bytes.clone()) {
                continue;
            }
            if bytes == ROOMS_STORAGE_KEY {
                has_blob = true;
            } else if let Some(owner) = parse_room_storage_key(&bytes) {
                room_keys.push((owner, bytes));
            }
        }

        // Per-room slots first: within one generation they are newer than a
        // leftover single blob.
        for (owner, storage_key) in room_keys {
            let Some(bytes) = legacy_get(&delegate, &storage_key).await else {
                continue;
            };
            let Ok(RoomSlot::Present(room)) = from_reader::<RoomSlot, _>(&bytes[..]) else {
                continue;
            };
            let cur = current_copy(&mut current, &owner).await.clone();
            if let CurrentCopy::Present(c) = &cur {
                if has_full_state(c) {
                    full_kept.insert(owner.to_bytes());
                }
            }
            let action = decide_present_copy(
                &CopyContext {
                    owner,
                    current: &cur,
                    current_rank,
                    newer_full_copy_kept: full_kept.contains(&owner.to_bytes()),
                    generation_resolved: resolved_here,
                    candidate_rank: rank,
                },
                &room,
            );
            match action {
                CopyAction::Keep => {
                    if has_full_state(&room) {
                        full_kept.insert(owner.to_bytes());
                    }
                }
                CopyAction::Compact => {
                    let (ok, freed) = match compacted_slot_bytes(&room) {
                        Some(new) => {
                            let freed = bytes.len().saturating_sub(new.len()) as u64;
                            (legacy_store(&delegate, &storage_key, new).await, freed)
                        }
                        None => (false, 0),
                    };
                    record(&mut report, action, ok, freed);
                }
                CopyAction::Delete => {
                    let ok = legacy_delete(&delegate, &storage_key).await;
                    record(&mut report, action, ok, bytes.len() as u64);
                }
            }
        }

        if has_blob {
            let Some(bytes) = legacy_get(&delegate, ROOMS_STORAGE_KEY).await else {
                continue;
            };
            let Ok(blob) = from_reader::<Rooms, _>(&bytes[..]) else {
                continue;
            };
            let owners: Vec<VerifyingKey> = blob
                .map
                .keys()
                .chain(blob.removed_rooms.iter())
                .copied()
                .collect();
            for owner in &owners {
                let cur = current_copy(&mut current, owner).await;
                if matches!(cur, CurrentCopy::Present(c) if has_full_state(c)) {
                    full_kept.insert(owner.to_bytes());
                }
            }
            let (action, kept) = plan_blob(
                &blob,
                &current,
                &full_kept,
                current_rank,
                resolved_here,
                rank,
            );
            for owner in kept {
                full_kept.insert(owner.to_bytes());
            }
            match action {
                BlobAction::Keep => {}
                BlobAction::Rewrite(new) => {
                    let freed = bytes.len().saturating_sub(new.len()) as u64;
                    let ok = legacy_store(&delegate, ROOMS_STORAGE_KEY, new).await;
                    record(&mut report, CopyAction::Compact, ok, freed);
                }
                BlobAction::Delete => {
                    let ok = legacy_delete(&delegate, ROOMS_STORAGE_KEY).await;
                    record(&mut report, CopyAction::Delete, ok, bytes.len() as u64);
                }
            }
        }
    }
    report
}

// ---------------------------------------------------------------------------
// Scheduling
// ---------------------------------------------------------------------------

/// The cleanup task has been started on this page load.
static SCHEDULED: AtomicBool = AtomicBool::new(false);
/// Passes run on this page load.
static PASSES: AtomicU32 = AtomicU32::new(0);

/// Passes per page load: one that can only compact (nothing committed yet),
/// and one after the save and the walk, which can delete.
const MAX_PASSES_PER_LOAD: u32 = 2;
/// How long to wait for the load (or the walk) to go quiet before a pass.
const QUIET_WAIT_MAX_MS: u64 = 180_000;
const QUIET_POLL_MS: u64 = 1_000;
/// Quiet must hold this long, so a pass does not start in a gap between two
/// waves of the load.
const QUIET_SETTLE_MS: u64 = 3_000;

/// Note that a legacy generation answered with room data, and start the
/// cleanup for this page load (once). Called from the sweep's merge paths.
pub(crate) fn note_legacy_data_seen() {
    if SCHEDULED.swap(true, Ordering::Relaxed) {
        return;
    }
    crate::util::safe_spawn_local(cleanup_task());
}

fn load_is_quiet() -> bool {
    chat_delegate::PENDING_LOADS.load(Ordering::Relaxed) == 0
}

fn load_and_walk_are_quiet() -> bool {
    load_is_quiet() && !migration_journal::walk_in_flight()
}

/// Wait until `quiet` has held for [`QUIET_SETTLE_MS`], or
/// [`QUIET_WAIT_MAX_MS`]. Whether it did.
///
/// The walk is serialised against a pass by the journal lock, not by this
/// wait, so a pass that starts while a walk is requested is still safe: the
/// first pass waits only for the load (a walk that cannot finish, say because
/// every write it makes is refused for quota, must not keep the cleanup that
/// frees the quota from ever running), and the second waits for the walk so
/// it sees the walk's resolutions, then runs regardless.
async fn wait_for(quiet: fn() -> bool) -> bool {
    let mut waited = 0u64;
    let mut quiet_for = 0u64;
    while waited < QUIET_WAIT_MAX_MS {
        crate::util::sleep(std::time::Duration::from_millis(QUIET_POLL_MS)).await;
        waited += QUIET_POLL_MS;
        if quiet() {
            quiet_for += QUIET_POLL_MS;
            if quiet_for >= QUIET_SETTLE_MS {
                return true;
            }
        } else {
            quiet_for = 0;
        }
    }
    false
}

/// One locked pass, if the page load's budget allows.
async fn locked_pass() -> Option<PassReport> {
    if PASSES.fetch_add(1, Ordering::Relaxed) >= MAX_PASSES_PER_LOAD {
        return None;
    }
    // Lock order: journal, then the slot fan-out. The sweep's per-room
    // migration takes only the slot lock and releases it before its journal
    // gate, and the walk takes only the journal lock, so no cycle exists.
    let _ops = migration_journal::lock_journal_ops().await;
    let _slots = super::response_handler::LEGACY_SLOT_FETCH.lock().await;
    let resolved = migration_journal::resolved_predecessors_now();
    let report = run_pass(resolved).await;
    info!(
        "legacy cleanup: compacted {} and deleted {} legacy room copies ({} failed, ~{} bytes freed)",
        report.compacted, report.deleted, report.failed, report.bytes_freed
    );
    Some(report)
}

async fn cleanup_task() {
    if !wait_for(load_is_quiet).await {
        warn!("legacy cleanup: the load did not go quiet; the next load retries");
        return;
    }
    let Some(first) = locked_pass().await else {
        return;
    };
    if !first.changed_anything() {
        return;
    }
    // Space was freed: commit the rooms the sweep merged (a save refused for
    // quota earlier left them unrecorded, so it writes them now), then let the
    // walk resolve the journal, and run a pass that can delete.
    match chat_delegate::save_rooms_to_delegate().await {
        Ok(()) => info!("legacy cleanup: rooms saved to the current delegate"),
        Err(e) => {
            warn!("legacy cleanup: saving rooms after the cleanup failed: {e}");
            return;
        }
    }
    migration_journal::poke_walk();
    if !wait_for(load_and_walk_are_quiet).await {
        info!("legacy cleanup: the walk is still running; deleting what is resolved so far");
    }
    let _ = locked_pass().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_data::test_room_with_messages;
    use ed25519_dalek::SigningKey;

    const CURRENT: u32 = 30;

    fn owner_sk() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn full_room(count: usize) -> RoomData {
        test_room_with_messages(&owner_sk(), &SigningKey::from_bytes(&[8u8; 32]), count, 5).0
    }

    fn ctx<'a>(
        current: &'a CurrentCopy,
        newer_full_copy_kept: bool,
        generation_resolved: bool,
    ) -> CopyContext<'a> {
        CopyContext {
            owner: owner_sk().verifying_key(),
            current,
            current_rank: CURRENT,
            newer_full_copy_kept,
            generation_resolved,
            candidate_rank: 10,
        }
    }

    #[test]
    fn compaction_keeps_every_unrecoverable_field() {
        let room = full_room(3);
        assert!(has_full_state(&room), "precondition: the fixture has state");
        let compacted = compact_room(&room);
        assert!(!has_full_state(&compacted));
        assert_eq!(critical_hash(&room), critical_hash(&compacted));
        // It round-trips as a slot the load reads.
        let bytes = compacted_slot_bytes(&room).unwrap();
        let back: RoomSlot = from_reader(&bytes[..]).unwrap();
        let RoomSlot::Present(back) = back else {
            panic!("compacted slot must stay Present");
        };
        assert_eq!(critical_hash(&back), critical_hash(&room));
        assert!(
            bytes.len() < {
                let mut full = Vec::new();
                ciborium::ser::into_writer(&RoomSlot::Present(Box::new(room.clone())), &mut full)
                    .unwrap();
                full.len()
            }
        );
    }

    /// The load must still read a compacted copy cleanly, in either order
    /// against a full copy, and end with the full snapshot.
    #[test]
    fn a_compacted_copy_merges_cleanly_with_a_full_one() {
        let owner = owner_sk().verifying_key();
        let full = full_room(3);
        let compacted = compact_room(&full);
        for (first, second) in [(&full, &compacted), (&compacted, &full)] {
            let mut rooms = single_room(owner, None);
            rooms.removed_rooms.clear();
            let mut ranks = MergeRanks::default();
            rooms
                .merge_from_source(
                    single_room(owner, Some(first)),
                    20,
                    MergeAuthority::OlderSnapshot,
                    &mut ranks,
                )
                .unwrap();
            rooms
                .merge_from_source(
                    single_room(owner, Some(second)),
                    10,
                    MergeAuthority::OlderSnapshot,
                    &mut ranks,
                )
                .unwrap();
            let merged = &rooms.map[&owner];
            assert_eq!(
                merged.room_state.recent_messages.messages.len(),
                3,
                "the full snapshot survives the merge"
            );
            assert_eq!(critical_hash(merged), critical_hash(&full));
        }
    }

    #[test]
    fn an_older_duplicate_under_a_kept_full_copy_is_compacted() {
        let unknown = CurrentCopy::Unknown;
        // The quota-full case: the current delegate's copy cannot be relied on,
        // the generation is pending, a newer generation keeps the full copy.
        assert_eq!(
            decide_present_copy(&ctx(&unknown, true, false), &full_room(3)),
            CopyAction::Compact
        );
    }

    #[test]
    fn the_newest_full_copy_is_never_shrunk() {
        let unknown = CurrentCopy::Unknown;
        let absent = CurrentCopy::Absent;
        // No newer full copy anywhere: this one is it, resolved or not.
        for resolved in [false, true] {
            assert_eq!(
                decide_present_copy(&ctx(&unknown, false, resolved), &full_room(3)),
                CopyAction::Keep
            );
            assert_eq!(
                decide_present_copy(&ctx(&absent, false, resolved), &full_room(3)),
                CopyAction::Keep
            );
        }
        // A current copy that holds only the default state is not a full copy.
        let placeholder = CurrentCopy::Present(Box::new(compact_room(&full_room(3))));
        assert_eq!(
            decide_present_copy(&ctx(&placeholder, false, true), &full_room(3)),
            CopyAction::Keep
        );
    }

    #[test]
    fn a_pending_generation_is_never_deleted_from() {
        let current = CurrentCopy::Present(Box::new(full_room(5)));
        assert_eq!(
            decide_present_copy(&ctx(&current, true, false), &full_room(3)),
            CopyAction::Compact,
            "pending: compact only, the key and its item count stay"
        );
        assert_eq!(
            decide_present_copy(&ctx(&current, true, false), &compact_room(&full_room(3))),
            CopyAction::Keep
        );
    }

    #[test]
    fn a_resolved_copy_the_current_delegate_covers_is_deleted() {
        let current = CurrentCopy::Present(Box::new(full_room(5)));
        assert_eq!(
            decide_present_copy(&ctx(&current, true, true), &full_room(3)),
            CopyAction::Delete
        );
        assert_eq!(
            decide_present_copy(&ctx(&current, true, true), &compact_room(&full_room(3))),
            CopyAction::Delete
        );
        let tomb = CurrentCopy::Tombstone;
        assert_eq!(
            decide_present_copy(&ctx(&tomb, false, true), &full_room(3)),
            CopyAction::Delete
        );
    }

    /// The data-loss guard: a legacy copy carrying an unrecoverable field the
    /// current delegate lacks (one the load's merge would add) is never
    /// deleted, only compacted, which keeps that field.
    #[test]
    fn a_copy_that_adds_an_unrecoverable_field_is_not_deleted() {
        let current = CurrentCopy::Present(Box::new(full_room(5)));
        let mut legacy = full_room(3);
        legacy.invitation_secrets.insert(4, [9u8; 32]);
        assert!(!redundant_under_current(
            owner_sk().verifying_key(),
            &current,
            CURRENT,
            &legacy,
            10
        ));
        let action = decide_present_copy(&ctx(&current, true, true), &legacy);
        assert_eq!(action, CopyAction::Compact);
        assert_eq!(
            critical_hash(&compact_room(&legacy)),
            critical_hash(&legacy),
            "compaction keeps the field the current copy lacks"
        );

        let mut nicknamed = full_room(3);
        nicknamed.self_nickname = Some("only here".into());
        let mut bare = full_room(5);
        bare.self_nickname = None;
        assert!(!redundant_under_current(
            owner_sk().verifying_key(),
            &CurrentCopy::Present(Box::new(bare)),
            CURRENT,
            &nicknamed,
            10
        ));
    }

    /// An older generation's copy under a DIFFERENT identity is discarded by
    /// the ranked merge (the current delegate outranks it, #527), so it is
    /// redundant.
    #[test]
    fn an_outranked_identity_is_redundant() {
        let current = CurrentCopy::Present(Box::new(full_room(5)));
        let other =
            test_room_with_messages(&owner_sk(), &SigningKey::from_bytes(&[3u8; 32]), 2, 5).0;
        assert!(redundant_under_current(
            owner_sk().verifying_key(),
            &current,
            CURRENT,
            &other,
            10
        ));
    }

    #[test]
    fn nothing_is_concluded_from_an_unread_current_copy() {
        for current in [CurrentCopy::Unknown, CurrentCopy::Absent] {
            assert!(!redundant_under_current(
                owner_sk().verifying_key(),
                &current,
                CURRENT,
                &full_room(3),
                10
            ));
            assert_ne!(
                decide_present_copy(&ctx(&current, true, true), &full_room(3)),
                CopyAction::Delete
            );
        }
    }

    fn blob_of(rooms: &[(VerifyingKey, RoomData)], removed: &[VerifyingKey]) -> Rooms {
        let mut blob = single_room(owner_sk().verifying_key(), None);
        blob.removed_rooms.clear();
        for (vk, room) in rooms {
            blob.map.insert(*vk, room.clone());
        }
        blob.removed_rooms.extend(removed.iter().copied());
        blob
    }

    #[test]
    fn a_blob_is_compacted_room_by_room_and_deleted_only_when_covered() {
        let owner = owner_sk().verifying_key();
        let blob = blob_of(&[(owner, full_room(3))], &[]);
        let mut current = HashMap::new();
        let kept: HashSet<[u8; 32]> = [owner.to_bytes()].into_iter().collect();

        // Pending generation, newer full copy kept: rewritten compacted.
        let (action, _) = plan_blob(&blob, &current, &kept, CURRENT, false, 10);
        let BlobAction::Rewrite(bytes) = action else {
            panic!("expected a rewrite, got {action:?}");
        };
        let back: Rooms = from_reader(&bytes[..]).unwrap();
        assert!(!has_full_state(&back.map[&owner]));
        assert_eq!(
            critical_hash(&back.map[&owner]),
            critical_hash(&blob.map[&owner])
        );

        // Nothing newer kept and nothing known about the current copy: kept.
        let (action, kept_full) = plan_blob(&blob, &current, &HashSet::new(), CURRENT, true, 10);
        assert_eq!(action, BlobAction::Keep);
        assert_eq!(kept_full, vec![owner]);

        // Resolved, covered by the current delegate: deleted.
        current.insert(
            owner.to_bytes(),
            CurrentCopy::Present(Box::new(full_room(5))),
        );
        let (action, _) = plan_blob(&blob, &current, &kept, CURRENT, true, 10);
        assert_eq!(action, BlobAction::Delete);

        // ...unless it carries a tombstone the current delegate knows nothing
        // of: then it stays (rewritten compacted).
        let other = SigningKey::from_bytes(&[2u8; 32]).verifying_key();
        let tombstoned = blob_of(&[(owner, full_room(3))], &[other]);
        let (action, _) = plan_blob(&tombstoned, &current, &kept, CURRENT, true, 10);
        assert!(matches!(action, BlobAction::Rewrite(_)), "got {action:?}");
    }
}
