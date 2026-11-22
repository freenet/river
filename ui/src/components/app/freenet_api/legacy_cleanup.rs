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
//! import, and every load re-runs the full legacy sweep, downloading every copy.
//!
//! # What a pass does
//!
//! The generations that answered with room data on this page load are visited
//! newest first. A legacy copy of a room is **compacted**, rewritten without its
//! `room_state` (the room contract's state: messages, members, configuration),
//! when a NEWER full copy of the room is kept: the current delegate's, or one a
//! newer generation keeps ([`decide_present_copy`]). Otherwise it is kept as is,
//! so the newest full snapshot of every room always survives. Everything else is
//! kept too: per-room tombstones (a few bytes) and compacted copies.
//!
//! `room_state` is the bulk of a copy's bytes and the one part
//! [`chat_delegate::critical_hash`] treats as a reconstructible cache.
//! Everything unrecoverable (`self_sk`, the invite chain, invitation secrets,
//! own purges, nickname, ...) is kept field for field, and the key stays. A
//! legacy `rooms_data` blob is rewritten the same way, room by room
//! ([`plan_blob`]), with its tombstones and view preferences untouched.
//!
//! # Why compaction and never deletion
//!
//! * **The journal.** freenet-migrate does not believe a predecessor that
//!   exports fewer items than an interrupted import saw (#44), so deleting a
//!   key from a pending generation keeps it pending, and the full sweep
//!   re-running, for good. A resolved generation is not safe either: an
//!   `ImportedWithRejections` entry reopens with its old count when River's
//!   import revision rises, and a `NoData` one when an import from it starts.
//!   Compaction keeps every key, so every count holds.
//! * **The ranked merge.** Which copy of a room a load keeps depends on rank
//!   provenance persisted in `rooms_meta` (#768) and, for some fields, on
//!   arrival order, so "the current copy covers this one" cannot be decided
//!   from the two copies alone. A compacted copy keeps every field the merge
//!   could ever take from it.
//!
//! A compacted copy costs a few kilobytes, so the duplicates stop costing
//! quota: eight full copies become one full copy and seven small ones.
//!
//! # The quota-full case, and why the order cannot lose data
//!
//! Compaction never needs a write to the current delegate, so it works when the
//! quota is full. After a pass that compacted anything the rooms are saved to
//! the current delegate and the walk is asked to resolve the journal; a second
//! pass then compacts the copies the newly committed current copy supersedes.
//! Interrupted at any point:
//!
//! * mid-compaction: a compacted copy keeps every unrecoverable field, and a
//!   newer full copy of the room exists;
//! * after compaction, before the save: the newest full copy is still in its
//!   legacy generation (or the current delegate), and the next load merges it
//!   as before.
//!
//! # Limits
//!
//! * `outbound_dms` and `rooms_meta` are left alone: they are small.
//! * The compaction is a plain `StoreRequest`. A generation's FULL copies are
//!   written only by builds for which it was the current delegate, all older
//!   than or equal to this one, so decoding and re-encoding one with this
//!   build's types drops no field. A compacted copy is never rewritten again.
//! * An old-build tab still writing to a superseded generation between this
//!   pass's read and its write would have that write replaced. Such tabs reload
//!   into the current build when they see the re-key (`generation_watch`), and
//!   the replacement keeps every unrecoverable field this pass read.
//! * Every legacy delegate WASM handles `StoreRequest` (in the protocol since
//!   the first chat delegate, March 2025, before the oldest registered
//!   generation). The node charges an overwrite only its size delta, so a
//!   smaller rewrite succeeds with the quota full.
//! * Only generations that answered the sweep or the walk with room data on
//!   this load are visited, so a pass costs nothing for the generations the
//!   node does not hold (a silent one would cost a 10 s timeout each).

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ciborium::from_reader;
use dioxus::logger::tracing::{info, warn};
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::{CodeHash, DelegateKey};
use river_core::chat_delegate::{ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg};

use super::migration_journal;
use crate::components::app::chat_delegate::{
    self, legacy_delegate_pairs, parse_room_storage_key, send_delegate_request,
    send_delegate_request_to, ROOMS_STORAGE_KEY,
};
use crate::room_data::{RoomData, RoomSlot, Rooms};

// ---------------------------------------------------------------------------
// Pure decisions
// ---------------------------------------------------------------------------

/// What the current delegate holds for one room, as this pass read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CurrentCopy {
    /// The read failed or did not decode: nothing may be concluded from it.
    Unknown,
    /// No slot.
    Absent,
    /// The user left the room.
    Tombstone,
    /// The room, with a room snapshot.
    Full,
    /// The room without a snapshot (compacted, or not yet fetched).
    Placeholder,
}

/// What to do with one legacy copy of a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyAction {
    Keep,
    Compact,
}

/// Whether `room` carries a room snapshot at all (a compacted or freshly
/// imported room carries the default, empty state).
pub(crate) fn has_full_state(room: &RoomData) -> bool {
    room.room_state != Default::default()
}

/// `room` without its reconstructible room snapshot. Every other field is
/// unchanged.
pub(crate) fn compact_room(room: &RoomData) -> RoomData {
    let mut compacted = room.clone();
    compacted.room_state = Default::default();
    compacted
}

/// Decide one legacy `Present` copy: compact a full copy when a newer full copy
/// of the room is kept, or the current delegate has tombstoned the room (the
/// load discards a legacy copy under it, and compaction still keeps every
/// unrecoverable field should the leave ever be undone).
pub(crate) fn decide_present_copy(
    newer_full_copy_kept: bool,
    current: CurrentCopy,
    candidate: &RoomData,
) -> CopyAction {
    let superseded = newer_full_copy_kept || current == CurrentCopy::Tombstone;
    if has_full_state(candidate) && superseded {
        CopyAction::Compact
    } else {
        CopyAction::Keep
    }
}

/// Plan a legacy generation's `rooms_data` blob room by room, as
/// [`decide_present_copy`] would each room. `Some(bytes)` rewrites it with the
/// eligible rooms compacted and everything else (tombstones, order,
/// notification modes) untouched; `None` keeps it. Also returns the rooms it
/// keeps in full.
pub(crate) fn plan_blob(
    blob: &Rooms,
    current: &HashMap<[u8; 32], CurrentCopy>,
    newer_full_kept: &HashSet<[u8; 32]>,
) -> (Option<Vec<u8>>, Vec<VerifyingKey>) {
    let mut kept_full = Vec::new();
    let mut compact = Vec::new();
    for (owner, room) in &blob.map {
        let cur = current
            .get(&owner.to_bytes())
            .copied()
            .unwrap_or(CurrentCopy::Unknown);
        match decide_present_copy(newer_full_kept.contains(&owner.to_bytes()), cur, room) {
            CopyAction::Compact => compact.push(*owner),
            CopyAction::Keep if has_full_state(room) => kept_full.push(*owner),
            CopyAction::Keep => {}
        }
    }
    if compact.is_empty() {
        return (None, kept_full);
    }
    let mut rewritten = blob.clone();
    for owner in &compact {
        if let Some(room) = rewritten.map.get_mut(owner) {
            room.room_state = Default::default();
        }
    }
    let mut bytes = Vec::new();
    match ciborium::ser::into_writer(&rewritten, &mut bytes) {
        Ok(()) => (Some(bytes), kept_full),
        Err(_) => (None, kept_full),
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

/// The delegate operations a pass needs, so the pass runs against a fake store
/// in tests.
pub(crate) trait CleanupIo {
    /// The keys `delegate` lists; `None` when it did not answer.
    async fn list(&self, delegate: &DelegateKey) -> Option<Vec<Vec<u8>>>;
    /// The value under `key` in `delegate`; `None` when absent or unanswered.
    async fn get(&self, delegate: &DelegateKey, key: &[u8]) -> Option<Vec<u8>>;
    /// Overwrite `key` in `delegate`. Whether it was stored.
    async fn store(&self, delegate: &DelegateKey, key: &[u8], value: Vec<u8>) -> bool;
    /// What the CURRENT delegate holds for `owner`.
    async fn current(&self, owner: &VerifyingKey) -> CurrentCopy;
}

/// What one pass changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PassReport {
    pub(crate) compacted: u32,
    pub(crate) failed: u32,
    /// Bytes no longer stored (value size deltas).
    pub(crate) bytes_freed: u64,
}

/// The current delegate's copy of `owner`, read once per pass.
async fn current_copy<IO: CleanupIo>(
    io: &IO,
    cache: &mut HashMap<[u8; 32], CurrentCopy>,
    owner: &VerifyingKey,
) -> CurrentCopy {
    if let Some(copy) = cache.get(&owner.to_bytes()) {
        return *copy;
    }
    let copy = io.current(owner).await;
    cache.insert(owner.to_bytes(), copy);
    copy
}

/// One pass over `generations` (registry indexes), newest first. The live
/// caller holds the journal lock and the legacy slot-fetch lock.
pub(crate) async fn run_pass<IO: CleanupIo>(io: &IO, generations: &BTreeSet<usize>) -> PassReport {
    let pairs = legacy_delegate_pairs();
    let mut report = PassReport::default();
    let mut current: HashMap<[u8; 32], CurrentCopy> = HashMap::new();
    // Rooms a full copy NEWER than the generation being visited is kept of.
    let mut full_kept: HashSet<[u8; 32]> = HashSet::new();
    for &index in generations.iter().rev() {
        let Some((key_bytes, code_hash)) = pairs.get(index) else {
            continue;
        };
        let delegate = DelegateKey::new(*key_bytes, CodeHash::new(*code_hash));
        // Silence concludes nothing: skip the generation.
        let Some(keys) = io.list(&delegate).await else {
            continue;
        };
        let mut seen = HashSet::new();
        let mut room_keys = Vec::new();
        let mut has_blob = false;
        for key in keys {
            if !seen.insert(key.clone()) {
                continue;
            }
            if key == ROOMS_STORAGE_KEY {
                has_blob = true;
            } else if let Some(owner) = parse_room_storage_key(&key) {
                room_keys.push((owner, key));
            }
        }

        // Per-room slots first: within one generation they are newer than a
        // leftover single blob.
        for (owner, storage_key) in room_keys {
            let Some(bytes) = io.get(&delegate, &storage_key).await else {
                continue;
            };
            let Ok(RoomSlot::Present(room)) = from_reader::<RoomSlot, _>(&bytes[..]) else {
                continue;
            };
            let cur = current_copy(io, &mut current, &owner).await;
            if cur == CurrentCopy::Full {
                full_kept.insert(owner.to_bytes());
            }
            match decide_present_copy(full_kept.contains(&owner.to_bytes()), cur, &room) {
                CopyAction::Keep => {
                    if has_full_state(&room) {
                        full_kept.insert(owner.to_bytes());
                    }
                }
                CopyAction::Compact => match compacted_slot_bytes(&room) {
                    Some(new) => {
                        let freed = bytes.len().saturating_sub(new.len()) as u64;
                        if io.store(&delegate, &storage_key, new).await {
                            report.compacted += 1;
                            report.bytes_freed += freed;
                        } else {
                            report.failed += 1;
                        }
                    }
                    None => report.failed += 1,
                },
            }
        }

        if has_blob {
            let Some(bytes) = io.get(&delegate, ROOMS_STORAGE_KEY).await else {
                continue;
            };
            let Ok(blob) = from_reader::<Rooms, _>(&bytes[..]) else {
                continue;
            };
            let owners: Vec<VerifyingKey> = blob.map.keys().copied().collect();
            for owner in &owners {
                if current_copy(io, &mut current, owner).await == CurrentCopy::Full {
                    full_kept.insert(owner.to_bytes());
                }
            }
            let (rewrite, kept) = plan_blob(&blob, &current, &full_kept);
            for owner in kept {
                full_kept.insert(owner.to_bytes());
            }
            if let Some(new) = rewrite {
                let freed = bytes.len().saturating_sub(new.len()) as u64;
                if io.store(&delegate, ROOMS_STORAGE_KEY, new).await {
                    report.compacted += 1;
                    report.bytes_freed += freed;
                } else {
                    report.failed += 1;
                }
            }
        }
    }
    report
}

/// The live node.
struct NodeIo;

impl CleanupIo for NodeIo {
    async fn list(&self, delegate: &DelegateKey) -> Option<Vec<Vec<u8>>> {
        match send_delegate_request_to(delegate.clone(), ChatDelegateRequestMsg::ListRequest).await
        {
            Ok(ChatDelegateResponseMsg::ListResponse { keys }) => {
                Some(keys.into_iter().map(|k| k.as_bytes().to_vec()).collect())
            }
            _ => None,
        }
    }

    async fn get(&self, delegate: &DelegateKey, key: &[u8]) -> Option<Vec<u8>> {
        let request = ChatDelegateRequestMsg::GetRequest {
            key: ChatDelegateKey::new(key.to_vec()),
        };
        match send_delegate_request_to(delegate.clone(), request).await {
            Ok(ChatDelegateResponseMsg::GetResponse { value, .. }) => value,
            _ => None,
        }
    }

    async fn store(&self, delegate: &DelegateKey, key: &[u8], value: Vec<u8>) -> bool {
        let request = ChatDelegateRequestMsg::StoreRequest {
            key: ChatDelegateKey::new(key.to_vec()),
            value,
        };
        matches!(
            send_delegate_request_to(delegate.clone(), request).await,
            Ok(ChatDelegateResponseMsg::StoreResponse { result: Ok(()), .. })
        )
    }

    async fn current(&self, owner: &VerifyingKey) -> CurrentCopy {
        let request = ChatDelegateRequestMsg::GetRequest {
            key: ChatDelegateKey::new(chat_delegate::room_storage_key(owner)),
        };
        match send_delegate_request(request).await {
            Ok(ChatDelegateResponseMsg::GetResponse { value: None, .. }) => CurrentCopy::Absent,
            Ok(ChatDelegateResponseMsg::GetResponse {
                value: Some(bytes), ..
            }) => match from_reader::<RoomSlot, _>(&bytes[..]) {
                Ok(RoomSlot::Present(room)) if has_full_state(&room) => CurrentCopy::Full,
                Ok(RoomSlot::Present(_)) => CurrentCopy::Placeholder,
                Ok(RoomSlot::Tombstone) => CurrentCopy::Tombstone,
                Err(_) => CurrentCopy::Unknown,
            },
            _ => CurrentCopy::Unknown,
        }
    }
}

// ---------------------------------------------------------------------------
// Scheduling
// ---------------------------------------------------------------------------

thread_local! {
    /// Registry indexes of the generations that answered with room data on
    /// this page load. Only these are visited.
    static GENERATIONS_WITH_DATA: RefCell<BTreeSet<usize>> = const { RefCell::new(BTreeSet::new()) };
}
/// The cleanup task has been started on this page load.
static SCHEDULED: AtomicBool = AtomicBool::new(false);
/// Passes run on this page load.
static PASSES: AtomicU32 = AtomicU32::new(0);

/// Passes per page load: one over what the load found, and one after the save
/// and the walk, which sees the rooms just committed to the current delegate.
const MAX_PASSES_PER_LOAD: u32 = 2;
/// How long to wait for the load (or the walk) to go quiet before a pass.
const QUIET_WAIT_MAX_MS: u64 = 180_000;
const QUIET_POLL_MS: u64 = 1_000;
/// Quiet must hold this long, so a pass does not start in a gap between two
/// waves of the load.
const QUIET_SETTLE_MS: u64 = 3_000;

/// Note that legacy generation `delegate` answered with room data (the sweep's
/// merge paths and the walk's reader call this), and start the cleanup for
/// this page load, once.
pub(crate) fn note_legacy_data_seen(delegate: &[u8]) {
    let Some(index) = legacy_delegate_pairs()
        .iter()
        .position(|(key, _)| key.as_slice() == delegate)
    else {
        return;
    };
    GENERATIONS_WITH_DATA.with(|g| g.borrow_mut().insert(index));
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
/// A pass and a walk are serialised by the journal lock, not by this wait. The
/// first pass waits only for the load: a walk that cannot finish (every write
/// it makes refused for quota, say) must not keep the pass that frees the
/// quota from running. The second waits for the walk it asked for, then runs
/// regardless.
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
    // Lock order: journal, then the slot fan-out. The sweep's per-room
    // migration takes only the slot lock and releases it before its journal
    // gate, and the walk takes only the journal lock, so no cycle exists.
    let _ops = migration_journal::lock_journal_ops().await;
    let _slots = super::response_handler::LEGACY_SLOT_FETCH.lock().await;
    if PASSES.fetch_add(1, Ordering::Relaxed) >= MAX_PASSES_PER_LOAD {
        return None;
    }
    let generations = GENERATIONS_WITH_DATA.with(|g| g.borrow().clone());
    let report = run_pass(&NodeIo, &generations).await;
    info!(
        "legacy cleanup: compacted {} legacy room copies in {} generation(s) ({} failed, ~{} bytes freed)",
        report.compacted,
        generations.len(),
        report.failed,
        report.bytes_freed
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
    if first.compacted == 0 {
        return;
    }
    // Space was freed: commit the rooms the sweep merged (a save refused for
    // quota earlier left them unrecorded, so it writes them now), then let the
    // walk import what the sweep could not and resolve the journal. A second
    // pass compacts the copies the newly committed current copies supersede.
    match chat_delegate::save_rooms_to_delegate().await {
        Ok(()) => info!("legacy cleanup: rooms saved to the current delegate"),
        Err(e) => warn!("legacy cleanup: saving rooms after the cleanup failed: {e}"),
    }
    migration_journal::poke_walk();
    if !wait_for(load_and_walk_are_quiet).await {
        info!("legacy cleanup: the walk is still running; compacting what is committed so far");
    }
    let _ = locked_pass().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_data::test_room_with_messages;
    use ed25519_dalek::SigningKey;

    fn owner_sk() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    /// A room with a snapshot and every unrecoverable field populated.
    fn full_room(owner: &SigningKey, count: usize) -> RoomData {
        let mut room =
            test_room_with_messages(owner, &SigningKey::from_bytes(&[8u8; 32]), count, 5).0;
        room.self_nickname = Some("me".into());
        room.invitation_secrets.insert(3, [5u8; 32]);
        room.own_purges.record(
            SigningKey::from_bytes(&[4u8; 32]).verifying_key().into(),
            Vec::new(),
        );
        room.invite_chain = room.self_authorized_member.clone().into_iter().collect();
        room.backfill_self_vk();
        room
    }

    fn encode<T: serde::Serialize>(value: &T) -> Vec<u8> {
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(value, &mut bytes).unwrap();
        bytes
    }

    fn slot_bytes(room: RoomData) -> Vec<u8> {
        encode(&RoomSlot::Present(Box::new(room)))
    }

    #[test]
    fn compaction_drops_only_the_room_snapshot() {
        let room = full_room(&owner_sk(), 3);
        assert!(has_full_state(&room), "precondition: the fixture has state");
        let compacted = compact_room(&room);
        assert!(!has_full_state(&compacted));
        // Every other field, compared directly, not through critical_hash.
        assert_eq!(compacted.self_sk, room.self_sk);
        assert_eq!(compacted.self_vk, room.self_vk);
        assert_eq!(compacted.self_nickname, room.self_nickname);
        assert_eq!(compacted.invitation_secrets, room.invitation_secrets);
        assert_eq!(encode(&compacted.own_purges), encode(&room.own_purges));
        assert_eq!(encode(&compacted.invite_chain), encode(&room.invite_chain));
        assert_eq!(
            encode(&compacted.self_authorized_member),
            encode(&room.self_authorized_member)
        );
        let mut blanked = room.clone();
        blanked.room_state = Default::default();
        assert_eq!(encode(&compacted), encode(&blanked));
        assert_eq!(
            chat_delegate::critical_hash(&compacted),
            chat_delegate::critical_hash(&room)
        );
        // It decodes as the slot a load reads, and is much smaller.
        let bytes = compacted_slot_bytes(&room).unwrap();
        let RoomSlot::Present(back) = from_reader::<RoomSlot, _>(&bytes[..]).unwrap() else {
            panic!("a compacted slot must stay Present");
        };
        assert_eq!(encode(&*back), encode(&compacted));
        assert!(bytes.len() < slot_bytes(room).len());
    }

    fn empty_rooms() -> Rooms {
        Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: HashSet::new(),
            notification_modes: HashMap::new(),
            room_order: Vec::new(),
            migrated_rooms: Vec::new(),
        }
    }

    /// The load must still read a compacted copy cleanly, in either order
    /// against a full one, and end with the full snapshot.
    #[test]
    fn a_compacted_copy_merges_cleanly_with_a_full_one() {
        use crate::room_data::{MergeAuthority, MergeRanks};
        let owner = owner_sk().verifying_key();
        let full = full_room(&owner_sk(), 3);
        let compacted = compact_room(&full);
        let one = |room: &RoomData| {
            let mut rooms = empty_rooms();
            rooms.map.insert(owner, room.clone());
            rooms
        };
        for (first, second) in [(&full, &compacted), (&compacted, &full)] {
            let mut rooms = empty_rooms();
            let mut ranks = MergeRanks::default();
            rooms
                .merge_from_source(one(first), 20, MergeAuthority::OlderSnapshot, &mut ranks)
                .unwrap();
            rooms
                .merge_from_source(one(second), 10, MergeAuthority::OlderSnapshot, &mut ranks)
                .unwrap();
            let merged = &rooms.map[&owner];
            assert_eq!(merged.room_state.recent_messages.messages.len(), 3);
            assert_eq!(
                chat_delegate::critical_hash(merged),
                chat_delegate::critical_hash(&full)
            );
        }
    }

    #[test]
    fn decide_compacts_only_under_a_newer_full_copy_or_a_tombstone() {
        use CurrentCopy::*;
        let full = full_room(&owner_sk(), 3);
        let compacted = compact_room(&full);
        for current in [Unknown, Absent, Full, Placeholder, Tombstone] {
            assert_eq!(
                decide_present_copy(true, current, &full),
                CopyAction::Compact,
                "{current:?}"
            );
            assert_eq!(
                decide_present_copy(true, current, &compacted),
                CopyAction::Keep
            );
            assert_eq!(
                decide_present_copy(false, current, &compacted),
                CopyAction::Keep
            );
        }
        // Nothing newer kept: this copy is the newest full snapshot.
        for current in [Unknown, Absent, Full, Placeholder] {
            assert_eq!(decide_present_copy(false, current, &full), CopyAction::Keep);
        }
        assert_eq!(
            decide_present_copy(false, Tombstone, &full),
            CopyAction::Compact
        );
    }

    #[test]
    fn a_blob_is_compacted_room_by_room_and_keeps_its_tombstones() {
        let a_sk = owner_sk();
        let b_sk = SigningKey::from_bytes(&[2u8; 32]);
        let (a, b) = (a_sk.verifying_key(), b_sk.verifying_key());
        let gone = SigningKey::from_bytes(&[3u8; 32]).verifying_key();
        let mut blob = empty_rooms();
        blob.map.insert(a, full_room(&a_sk, 3));
        blob.map.insert(b, full_room(&b_sk, 2));
        blob.removed_rooms.insert(gone);
        blob.room_order = vec![b, a];
        let kept: HashSet<[u8; 32]> = [a.to_bytes()].into_iter().collect();

        let (rewrite, kept_full) = plan_blob(&blob, &HashMap::new(), &kept);
        let back: Rooms = from_reader(&rewrite.expect("a rewrite")[..]).unwrap();
        assert!(!has_full_state(&back.map[&a]), "a has a newer full copy");
        assert!(has_full_state(&back.map[&b]), "b's only full copy is kept");
        assert_eq!(kept_full, vec![b]);
        assert!(back.removed_rooms.contains(&gone));
        assert_eq!(back.room_order, vec![b, a]);
        assert_eq!(
            encode(&compact_room(&blob.map[&a])),
            encode(&back.map[&a]),
            "the compacted room keeps every other field"
        );

        let (rewrite, _) = plan_blob(&blob, &HashMap::new(), &HashSet::new());
        assert_eq!(rewrite, None, "nothing superseded: kept as is");
    }

    type KvStore = HashMap<Vec<u8>, Vec<u8>>;

    /// A fake node: legacy generations' key-value stores and the current
    /// delegate's copies. Stores are applied, and every store is recorded.
    #[derive(Default)]
    struct FakeIo {
        legacy: RefCell<HashMap<[u8; 32], KvStore>>,
        current: HashMap<[u8; 32], CurrentCopy>,
        silent: HashSet<[u8; 32]>,
        stores: RefCell<Vec<[u8; 32]>>,
    }

    impl FakeIo {
        fn put(&self, generation: usize, owner: &VerifyingKey, value: Vec<u8>) {
            let dk = legacy_delegate_pairs()[generation].0;
            self.legacy
                .borrow_mut()
                .entry(dk)
                .or_default()
                .insert(chat_delegate::room_storage_key(owner), value);
        }

        fn slot(&self, generation: usize, owner: &VerifyingKey) -> RoomSlot {
            let dk = legacy_delegate_pairs()[generation].0;
            let bytes = self.legacy.borrow()[&dk][&chat_delegate::room_storage_key(owner)].clone();
            from_reader(&bytes[..]).unwrap()
        }

        fn is_full(&self, generation: usize, owner: &VerifyingKey) -> bool {
            matches!(self.slot(generation, owner), RoomSlot::Present(r) if has_full_state(&r))
        }
    }

    fn key_of(delegate: &DelegateKey) -> [u8; 32] {
        delegate.bytes().try_into().unwrap()
    }

    impl CleanupIo for FakeIo {
        async fn list(&self, delegate: &DelegateKey) -> Option<Vec<Vec<u8>>> {
            let dk = key_of(delegate);
            if self.silent.contains(&dk) {
                return None;
            }
            Some(
                self.legacy
                    .borrow()
                    .get(&dk)
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default(),
            )
        }
        async fn get(&self, delegate: &DelegateKey, key: &[u8]) -> Option<Vec<u8>> {
            self.legacy
                .borrow()
                .get(&key_of(delegate))?
                .get(key)
                .cloned()
        }
        async fn store(&self, delegate: &DelegateKey, key: &[u8], value: Vec<u8>) -> bool {
            let dk = key_of(delegate);
            self.stores.borrow_mut().push(dk);
            self.legacy
                .borrow_mut()
                .entry(dk)
                .or_default()
                .insert(key.to_vec(), value);
            true
        }
        async fn current(&self, owner: &VerifyingKey) -> CurrentCopy {
            self.current
                .get(&owner.to_bytes())
                .copied()
                .unwrap_or(CurrentCopy::Unknown)
        }
    }

    /// The live case: eight generations hold a full copy of one room, the
    /// current delegate's copy cannot be read (every write to it is refused).
    /// The pass needs no current-delegate write: the newest copy stays full,
    /// the seven older ones are compacted, nothing unrecoverable changes.
    #[test]
    fn quota_full_pass_compacts_older_duplicates_without_a_current_write() {
        let sk = owner_sk();
        let owner = sk.verifying_key();
        let room = full_room(&sk, 3);
        let io = FakeIo::default();
        let generations: BTreeSet<usize> = (10..18).collect();
        for &g in &generations {
            io.put(g, &owner, slot_bytes(room.clone()));
        }
        let report = futures::executor::block_on(run_pass(&io, &generations));
        assert_eq!((report.compacted, report.failed), (7, 0));
        assert!(report.bytes_freed > 0);
        assert!(io.is_full(17, &owner), "the newest full copy survives");
        for g in 10..17 {
            let RoomSlot::Present(copy) = io.slot(g, &owner) else {
                panic!("generation {g} must stay Present");
            };
            assert_eq!(
                encode(&*copy),
                encode(&compact_room(&room)),
                "generation {g}"
            );
        }
        // The stores went to the older generations, newest first.
        let expected: Vec<[u8; 32]> = (10..17)
            .rev()
            .map(|g| legacy_delegate_pairs()[g].0)
            .collect();
        assert_eq!(*io.stores.borrow(), expected);

        // A second pass changes nothing more.
        let again = futures::executor::block_on(run_pass(&io, &generations));
        assert_eq!(again, PassReport::default());
    }

    /// Once the current delegate holds the room in full, even the newest legacy
    /// copy is compacted. A silent generation is skipped, a room only older
    /// generations hold keeps its newest full copy, and tombstones stay.
    #[test]
    fn a_committed_current_copy_supersedes_every_legacy_copy() {
        let a_sk = owner_sk();
        let b_sk = SigningKey::from_bytes(&[2u8; 32]);
        let (a, b) = (a_sk.verifying_key(), b_sk.verifying_key());
        let mut io = FakeIo::default();
        io.current.insert(a.to_bytes(), CurrentCopy::Full);
        io.put(12, &a, slot_bytes(full_room(&a_sk, 3)));
        io.put(11, &a, slot_bytes(full_room(&a_sk, 2)));
        io.put(11, &b, slot_bytes(full_room(&b_sk, 2)));
        io.put(10, &b, slot_bytes(full_room(&b_sk, 1)));
        io.put(9, &b, slot_bytes(full_room(&b_sk, 1)));
        io.put(9, &a, encode(&RoomSlot::Tombstone));
        io.silent.insert(legacy_delegate_pairs()[10].0);
        let generations: BTreeSet<usize> = [9, 10, 11, 12].into_iter().collect();
        let report = futures::executor::block_on(run_pass(&io, &generations));
        assert!(
            !io.is_full(12, &a) && !io.is_full(11, &a),
            "current supersedes both"
        );
        assert!(io.is_full(11, &b), "b's newest full copy stays");
        assert!(io.is_full(10, &b), "a silent generation is not touched");
        assert!(!io.is_full(9, &b));
        assert!(
            matches!(io.slot(9, &a), RoomSlot::Tombstone),
            "tombstones stay"
        );
        assert_eq!(report.compacted, 3);
    }

    #[test]
    fn only_registered_generations_are_noted() {
        note_legacy_data_seen(&[0xEE; 32]);
        assert!(GENERATIONS_WITH_DATA.with(|g| g.borrow().is_empty()));
        note_legacy_data_seen(&legacy_delegate_pairs()[3].0);
        assert!(GENERATIONS_WITH_DATA.with(|g| g.borrow().contains(&3)));
    }
}
